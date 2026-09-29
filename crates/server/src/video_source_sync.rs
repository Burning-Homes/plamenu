//! Periodic native `ActivityPub` channel/playlist copies. No companion downloader,
//! automatic publication, identity impersonation, or unbounded feed crawl.
use crate::{AppState, error::ApiError};
use plamenu_db::{video_draft, video_source_sync as db};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};
use tokio::task::JoinHandle;

const MAX_PAGES: usize = 4;
const MAX_CANDIDATES: usize = 20;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_REQUESTS: usize = 48;

struct Budget {
    used: usize,
    requests: usize,
    deadline: tokio::time::Instant,
}

fn invalid() -> ApiError {
    ApiError::Unprocessable("The public source feed could not be checked safely".into())
}
fn value_id(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))
}
fn same_origin(root: &str, other: &str) -> bool {
    match (url::Url::parse(root), url::Url::parse(other)) {
        (Ok(a), Ok(b)) => {
            a.origin() == b.origin()
                && b.username().is_empty()
                && b.password().is_none()
                && b.fragment().is_none()
        }
        _ => false,
    }
}
async fn fetch(
    state: &AppState,
    source: &db::Source,
    scope_root: &str,
    url: &str,
    budget: &mut Budget,
) -> Result<Value, ApiError> {
    let root = scope_root;
    if tokio::time::Instant::now() >= budget.deadline
        || state.shutdown.is_cancelled()
        || budget.requests >= MAX_REQUESTS
    {
        return Err(invalid());
    }
    budget.requests += 1;
    crate::video_source_import::validate_url(url, state.config.allow_private_fetch)?;
    if !same_origin(root, url)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, url).await?
    {
        return Err(invalid());
    }
    if !db::renew(&state.pool, source).await? {
        return Err(ApiError::Conflict("Source check stopped".into()));
    }
    // Select only network work: never suspend a DB transaction while its
    // lease renewal waits on a row locked by that same suspended future.
    let request = state.federation.fetch_activitypub_same_origin(url);
    tokio::pin!(request);
    let mut renew = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep_until(budget.deadline);
    tokio::pin!(deadline);
    let response = loop {
        tokio::select! {
            response=&mut request=>break response.map_err(|_|invalid())?,
            _=renew.tick()=>if !db::renew(&state.pool,source).await?{return Err(ApiError::Conflict("Source check stopped".into()));},
            ()=&mut deadline=>return Err(invalid()),
            ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Source check stopped".into())),
        }
    };
    budget.used = budget.used.saturating_add(response.body_bytes);
    if response.body_bytes > 2 * 1024 * 1024
        || budget.used > MAX_BYTES
        || !same_origin(root, &response.final_url)
    {
        return Err(invalid());
    }
    let value = response.document.ok_or_else(invalid)?;
    if value["id"].as_str() != Some(url) {
        return Err(invalid());
    }
    Ok(value)
}

struct Candidate {
    uri: String,
    page: String,
    channel: Option<String>,
    position: i32,
}
struct Batch {
    candidates: Vec<Candidate>,
    next: Option<String>,
    next_position: i32,
    bytes: Budget,
}

/// Fetch only bounded metadata. A saved page cursor lets large source feeds
/// advance over successive checks rather than silently dropping older entries.
#[allow(
    clippy::too_many_lines,
    reason = "Bounded canonical collection traversal shares its cursor, byte budget and visited set"
)]
async fn collect(state: &AppState, source: &db::Source) -> Result<Batch, ApiError> {
    let root = &source.source_url;
    if url::Url::parse(root).map_err(|_| invalid())?.host_str()
        == Some(state.config.domain.as_str())
    {
        return Err(invalid());
    }
    let mut bytes = Budget {
        used: 0,
        requests: 0,
        deadline: tokio::time::Instant::now() + Duration::from_secs(45),
    };
    let document = fetch(state, source, root, root, &mut bytes).await?;
    let (collection, channel, mut page) = match document["type"].as_str() {
        Some("Group" | "Person" | "Service" | "Application") => {
            let outbox = value_id(&document["outbox"])
                .ok_or_else(invalid)?
                .to_owned();
            let collection = fetch(state, source, root, &outbox, &mut bytes).await?;
            if !matches!(
                collection["type"].as_str(),
                Some("OrderedCollection" | "Collection")
            ) {
                return Err(invalid());
            }
            (outbox, Some(root.clone()), collection)
        }
        Some("Playlist") => {
            let public = ["to", "cc"]
                .iter()
                .any(|key| addressed_public(&document[*key]));
            if !public {
                return Err(invalid());
            }
            (root.clone(), None, document)
        }
        _ => return Err(invalid()),
    };
    let mut page_url = collection.clone();
    if let Some(cursor) = source.cursor_url.as_deref() {
        page = fetch(state, source, root, cursor, &mut bytes).await?;
        if cursor != collection && value_id(&page["partOf"]) != Some(collection.as_str()) {
            return Err(invalid());
        }
        page_url = cursor.to_owned();
    }
    let mut visited = HashSet::from([page_url.clone()]);
    let mut candidates = Vec::new();
    let mut identities = HashSet::new();
    for page_index in 0..MAX_PAGES {
        if let Some(entries) = page.get("orderedItems").or_else(|| page.get("items")) {
            let entries = entries
                .as_array()
                .filter(|entries| entries.len() <= 100)
                .ok_or_else(invalid)?;
            let start = if page_index == 0 && source.cursor_url.is_some() {
                usize::try_from(source.cursor_position).unwrap_or(0)
            } else {
                0
            };
            for (position, entry) in entries.iter().enumerate().skip(start) {
                let target = match entry["type"].as_str() {
                    Some("Announce" | "Create") => {
                        if channel.as_deref() != value_id(&entry["actor"]) {
                            continue;
                        }
                        value_id(&entry["object"])
                    }
                    Some("PlaylistElement") => value_id(&entry["url"]),
                    _ => value_id(entry),
                };
                let Some(uri) = target else {
                    continue;
                };
                crate::video_source_import::validate_url(uri, state.config.allow_private_fetch)?;
                if !identities.insert(uri.to_owned()) {
                    continue;
                }
                let mut conn = state
                    .pool
                    .acquire()
                    .await
                    .map_err(plamenu_db::DbError::from)?;
                if db::seen(&mut conn, source.id, uri).await? {
                    continue;
                }
                drop(conn);
                if candidates.len() >= MAX_CANDIDATES {
                    return Ok(Batch {
                        candidates,
                        next: Some(page_url),
                        next_position: i32::try_from(position).unwrap_or(100),
                        bytes,
                    });
                }
                candidates.push(Candidate {
                    uri: uri.to_owned(),
                    page: page_url.clone(),
                    channel: channel.clone(),
                    position: i32::try_from(position).unwrap_or(100),
                });
            }
        }
        let next = value_id(&page["next"])
            .or_else(|| {
                (page_url == collection)
                    .then(|| value_id(&page["first"]))
                    .flatten()
            })
            .map(str::to_owned);
        let Some(next) = next else {
            return Ok(Batch {
                candidates,
                next: None,
                next_position: 0,
                bytes,
            });
        };
        if !visited.insert(next.clone()) {
            return Err(invalid());
        }
        if page_index + 1 == MAX_PAGES {
            return Ok(Batch {
                candidates,
                next: Some(next),
                next_position: 0,
                bytes,
            });
        }
        page = fetch(state, source, root, &next, &mut bytes).await?;
        if value_id(&page["partOf"]) != Some(collection.as_str()) {
            return Err(invalid());
        }
        page_url = next;
    }
    unreachable!("bounded loop always returns at its last page")
}

fn addressed_public(value: &Value) -> bool {
    value.as_str() == Some(plamenu_ap::activity::PUBLIC)
        || value.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|v| v.as_str() == Some(plamenu_ap::activity::PUBLIC))
        })
}

fn labels(media: i64, value: &Value) -> plamenu_db::video_metadata::Metadata {
    let identifier = |key: &str, max: i32| {
        value[key]["identifier"]
            .as_str()
            .and_then(|v| v.parse::<i32>().ok())
            .filter(|v| (1..=max).contains(v))
    };
    let licence = identifier("licence", 9).filter(|id| {
        *id == 9 || crate::video_metadata::licence_url(*id) == value["licence"]["url"].as_str()
    });
    let warning = crate::filters::plain_text(value["summary"].as_str().unwrap_or_default())
        .chars()
        .take(500)
        .collect::<String>();
    plamenu_db::video_metadata::Metadata {
        media_id: media,
        category: identifier("category", 18),
        licence,
        licence_version_known: licence.is_some(),
        sensitive: value["sensitive"] == true || !warning.is_empty(),
        spoiler_text: warning,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "One bounded batch maps verified objects into atomically reserved private imports"
)]
async fn process(state: &AppState, source: &db::Source) -> Result<(), ApiError> {
    let mut batch = collect(state, source).await?;
    let mut result = "ok";
    for candidate in batch.candidates {
        if state.shutdown.is_cancelled() {
            return Err(ApiError::Conflict("Source check stopped".into()));
        }
        if !db::renew(&state.pool, source).await? {
            return Err(ApiError::Conflict("Source check stopped".into()));
        }
        let mut value = fetch(
            state,
            source,
            &candidate.uri,
            &candidate.uri,
            &mut batch.bytes,
        )
        .await?;
        let mut uri = candidate.uri.clone();
        if value["type"] == "PlaylistElement" {
            // PeerTube playlist pages use element IRIs, not Video IRIs. Permit
            // exactly one canonical element hop, only for the source playlist.
            if candidate.channel.is_some() || !same_origin(&source.source_url, &uri) {
                return Err(invalid());
            }
            uri = value_id(&value["url"]).ok_or_else(invalid)?.to_owned();
            crate::video_source_import::validate_url(&uri, state.config.allow_private_fetch)?;
            let mut conn = state
                .pool
                .acquire()
                .await
                .map_err(plamenu_db::DbError::from)?;
            if db::seen(&mut conn, source.id, &uri).await? {
                continue;
            }
            drop(conn);
            value = fetch(state, source, &uri, &uri, &mut batch.bytes).await?;
        }
        if let Some(channel) = &candidate.channel
            && !plamenu_ap::activity::attributed_to_ids(&value).any(|id| id == channel)
        {
            continue;
        }
        if crate::video_source_import::progressive_source(&value).is_err() {
            continue;
        }
        let id = plamenu_db::id::next();
        let title = value["name"]
            .as_str()
            .unwrap_or("Imported video")
            .trim()
            .chars()
            .take(120)
            .collect::<String>();
        let title = if title.chars().count() < 3 {
            "Imported video"
        } else {
            &title
        };
        let description = crate::filters::plain_text(value["content"].as_str().unwrap_or_default())
            .chars()
            .take(7500)
            .collect::<String>();
        let description = format!("{description}\n\nSource: {uri}");
        let key = format!("{id}.orig");
        let config = state.config.native_video;
        let new = video_draft::NewDraft {
            media_id: id,
            account_id: source.account_id,
            channel_id: source.channel_id,
            title,
            description: &description,
            language: None,
            visibility: "unlisted",
            source_file: &key,
            source_fingerprint: None,
            poster_time_seconds: 0.0,
            media_description: None,
            media_transcript: None,
            caption_vtt: None,
            reserved_bytes: i64::try_from(config.max_upload_bytes()).unwrap_or(i64::MAX),
        };
        let limits = video_draft::AdmissionLimits {
            unfinished_per_account: i64::from(config.unfinished_drafts),
            retained_source_bytes_per_account: config.storage_limits().per_account,
            pending_source_bytes_global: i64::try_from(
                config.pending_gib * crate::config::NativeVideoConfig::GIB,
            )
            .unwrap_or(i64::MAX),
            storage: config.storage_limits(),
            disk_available_bytes: crate::storage::available_bytes(&state.config.media_dir)
                .map_err(|e| ApiError::Internal(Box::new(e)))?,
            min_free_bytes: config.min_free_gib * crate::config::NativeVideoConfig::GIB,
            processing_reserved_bytes: state.video_resources.usage().1,
        };
        match db::enqueue(
            &state.pool,
            source,
            &uri,
            &new,
            &limits,
            &labels(id, &value),
        )
        .await?
        {
            db::Enqueued::Copied(_) | db::Enqueued::Seen => {}
            db::Enqueued::Quota => {
                result = "quota";
                batch.next = Some(candidate.page);
                batch.next_position = candidate.position;
                break;
            }
            db::Enqueued::HistoryLimit => {
                result = "history_limit";
                batch.next = Some(candidate.page);
                batch.next_position = candidate.position;
                break;
            }
            db::Enqueued::Lost => return Err(ApiError::Conflict("Source check stopped".into())),
        }
    }
    if !db::finish_at(
        &state.pool,
        source,
        result,
        batch.next.as_deref(),
        batch.next_position,
    )
    .await?
    {
        return Err(ApiError::Conflict("Source check stopped".into()));
    }
    Ok(())
}

pub async fn run_one(state: &AppState) -> Result<bool, ApiError> {
    let Some(source) = db::claim(&state.pool).await? else {
        return Ok(false);
    };
    let result = process(state, &source).await;
    if result.is_err() && !state.shutdown.is_cancelled() {
        db::finish_at(
            &state.pool,
            &source,
            "failed",
            source.cursor_url.as_deref(),
            source.cursor_position,
        )
        .await?;
    }
    result.map(|()| true)
}

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if run_one(&state).await.is_err() {
                tracing::warn!(
                    "native video source check failed; private source settings show its result"
                );
            }
            if !crate::workers::pause(&state, Duration::from_secs(15)).await {
                return;
            }
        }
    })
}
