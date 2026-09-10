//! Keeping a remote live broadcast's state honest without polling for it.
//!
//! `PeerTube` federates every live-lifecycle transition — announced, on air,
//! over — as an ordinary `Update`, so a **followed** channel's streams are
//! exact and free: [`crate::ingest::update_remote_note`] applies them as they
//! arrive. A video that was merely *resolved* (someone pasted a link) gets no
//! such activity, and would otherwise sit at whatever state it had the moment
//! it was first fetched — announced forever, or "live" long after the stream
//! ended.
//!
//! Rather than run a poller, the state is refreshed where a viewer actually
//! asks for it: opening the post, or pressing play. That keeps the rule the
//! rest of the media stack lives by — *never more origin load than watching on
//! the origin directly*. A refresh is one small JSON fetch, throttled per
//! broadcast so N simultaneous viewers cost one, and suppressed with the usual
//! escalating backoff when the origin stops answering.
//!
//! The fetch itself is [`crate::ingest::refresh_remote_status`], not a
//! bespoke live-only path, so a broadcast that has quietly become something
//! else — a live that ended and published its replay, and is now an ordinary
//! video with a real rendition ladder — is re-ingested correctly rather than
//! being mislabelled as still on air.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use plamenu_db::{media, media_fetch_failure, remote_stream_source, status};
use serde_json::Value;

use crate::error::ApiError;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

/// Minimum spacing between two origin checks of the same broadcast. Short
/// enough that pressing play on a stream that just started works, long enough
/// that a busy timeline cannot turn into a poll loop.
const REFRESH_INTERVAL: Duration = Duration::from_secs(20);
/// Floor for the failure backoff, which escalates from there and eventually
/// abandons the row outright — a peer that vanishes forever stops costing us.
const FAILURE_COOLDOWN: Duration = Duration::from_mins(1);
/// Failure-suppression namespace, distinct from the HLS proxy's lanes.
const KIND: &str = "live-refresh";
/// Bound on the throttle map. Live attachments are few; the cap only exists so
/// a pathological instance cannot grow it without limit.
const THROTTLE_MAX: usize = 4_096;

/// status id → when we last asked its origin about the broadcast.
static LAST_CHECK: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Claims the right to check `status_id`, returning false when the previous
/// check is still fresh. Claiming up front is what collapses a herd: the first
/// caller through takes the slot and every concurrent one backs off instead of
/// queueing behind it.
fn claim(status_id: i64) -> bool {
    let mut map = LAST_CHECK.lock_or_recover();
    if map
        .get(&status_id)
        .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
    {
        return false;
    }
    if map.len() >= THROTTLE_MAX {
        map.retain(|_, last| last.elapsed() < REFRESH_INTERVAL);
    }
    map.insert(status_id, Instant::now());
    true
}

/// Whether a broadcast in this state can still change on the origin.
///
/// A finished one-off live is terminal and never re-checked. A permanent live
/// re-arms for its next session, so even `ended` stays refreshable for one.
#[must_use]
pub fn refreshable(live_state: Option<&str>, permanent: bool) -> bool {
    match live_state {
        Some("waiting" | "live") => true,
        Some("ended") => permanent,
        _ => false,
    }
}

/// Brings any live attachments of `status_ids` up to date.
///
/// Best-effort throughout: a stale badge is not worth failing a render for, so
/// every error is swallowed.
pub async fn refresh_statuses(state: &AppState, status_ids: &[i64]) {
    if status_ids.is_empty() {
        return;
    }
    let Ok(per_status) = media::for_statuses(&state.pool, status_ids).await else {
        return;
    };
    for (_status_id, items) in per_status {
        if let Some(item) = items
            .iter()
            .find(|item| refreshable(item.live_state.as_deref(), item.live_permanent))
        {
            refresh_for_media(state, item.id).await;
        }
    }
}

/// Finds refreshable live attachments in status entities that have already
/// been rendered. Timeline callers use this instead of querying the media
/// table speculatively, keeping ordinary (non-live) feeds on their existing
/// query budget.
fn rendered_live_media_ids(entities: &[Value]) -> BTreeSet<i64> {
    fn collect(entity: &Value, ids: &mut BTreeSet<i64>) {
        if let Some(attachments) = entity.get("media_attachments").and_then(Value::as_array) {
            for attachment in attachments {
                let live = attachment.get("live");
                let state = live
                    .and_then(|value| value.get("state"))
                    .and_then(Value::as_str);
                let permanent = live
                    .and_then(|value| value.get("permanent"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if refreshable(state, permanent)
                    && let Some(id) = attachment
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| id.parse().ok())
                {
                    ids.insert(id);
                }
            }
        }
        for nested in ["reblog", "quote"] {
            if let Some(status) = entity.get(nested).filter(|value| value.is_object()) {
                collect(status, ids);
            }
        }
    }

    let mut ids = BTreeSet::new();
    for entity in entities {
        collect(entity, &mut ids);
    }
    ids
}

/// Refreshes only live attachments proven to exist in rendered entities.
/// Returns whether a row changed so the caller can re-render exact state in
/// the same response.
pub async fn refresh_rendered_statuses(state: &AppState, entities: &[Value]) -> bool {
    let mut refreshed = false;
    for media_id in rendered_live_media_ids(entities) {
        refreshed |= refresh_for_media(state, media_id).await;
    }
    refreshed
}

/// Brings the broadcast a single media attachment belongs to up to date —
/// the entry point for the playback routes, which know a media id and nothing
/// else. Returns whether anything was re-read, so the caller can reload the
/// row it may have just invalidated.
pub async fn refresh_for_media(state: &AppState, media_id: i64) -> bool {
    let Ok(rows) = media::find_by_ids(&state.pool, &[media_id]).await else {
        return false;
    };
    let Some(item) = rows.into_iter().next() else {
        return false;
    };
    if !refreshable(item.live_state.as_deref(), item.live_permanent) {
        return false;
    }
    if remote_stream_source::find_by_account(&state.pool, item.account_id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return refresh_owncast(state, &item).await;
    }
    let Some(status_id) = item.status_id else {
        return false;
    };
    refresh_status(state, status_id).await
}

/// `Owncast`'s Note itself is immutable; its public `/api/status` endpoint is
/// the authoritative live signal. It uses the same viewer-bound throttle and
/// failure backoff as the generic `ActivityPub` refresh path.
async fn refresh_owncast(state: &AppState, item: &media::Media) -> bool {
    if !claim(item.id) {
        return false;
    }
    if media_fetch_failure::is_cooling_down(&state.pool, KIND, item.id, FAILURE_COOLDOWN)
        .await
        .unwrap_or(true)
    {
        return false;
    }
    match crate::owncast::refresh_media(state, item).await {
        Ok(refreshed) => {
            media_fetch_failure::clear(&state.pool, KIND, item.id)
                .await
                .ok();
            refreshed
        }
        Err(error) => {
            tracing::debug!(%error, media = item.id, "Owncast live state refresh failed");
            media_fetch_failure::record(&state.pool, KIND, item.id)
                .await
                .ok();
            false
        }
    }
}

/// Re-reads one broadcast's status from its origin, if the throttle and the
/// failure backoff both allow it.
async fn refresh_status(state: &AppState, status_id: i64) -> bool {
    if !claim(status_id) {
        return false;
    }
    match check(state, status_id).await {
        Ok(refreshed) => {
            media_fetch_failure::clear(&state.pool, KIND, status_id)
                .await
                .ok();
            refreshed
        }
        Err(error) => {
            tracing::debug!(%error, status = status_id, "live state refresh failed");
            media_fetch_failure::record(&state.pool, KIND, status_id)
                .await
                .ok();
            false
        }
    }
}

async fn check(state: &AppState, status_id: i64) -> Result<bool, ApiError> {
    if media_fetch_failure::is_cooling_down(&state.pool, KIND, status_id, FAILURE_COOLDOWN).await? {
        return Ok(false);
    }
    let Some(existing) = status::find_by_id(&state.pool, status_id).await? else {
        return Ok(false);
    };
    let Some(uri) = existing.uri.clone() else {
        return Ok(false);
    };
    Ok(crate::ingest::refresh_remote_status(state, &existing, &uri)
        .await?
        .is_some())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::rendered_live_media_ids;

    #[test]
    fn finds_refreshable_media_in_statuses_and_nested_boosts() {
        let entities = vec![json!({
            "media_attachments": [
                {"id": "1", "live": {"state": "ended", "permanent": false}},
                {"id": "2", "live": {"state": "waiting", "permanent": false}}
            ],
            "reblog": {
                "media_attachments": [
                    {"id": "3", "live": {"state": "live", "permanent": true}},
                    {"id": "4"}
                ]
            },
            "quote": {
                "media_attachments": [
                    {"id": "5", "live": {"state": "ended", "permanent": true}}
                ]
            }
        })];

        assert_eq!(
            rendered_live_media_ids(&entities)
                .into_iter()
                .collect::<Vec<_>>(),
            vec![2, 3, 5]
        );
    }
}
