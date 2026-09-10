//! Owncast's small, explicitly detected ActivityPub/HLS bridge.
//!
//! Owncast federates a go-live notification as an ordinary `Note`; the Note
//! contains only the homepage and a preview image. The stable HLS endpoint is
//! advertised by `WebFinger`, while `/api/status` is the authoritative online
//! signal. We retain those two public hints only after `NodeInfo` positively
//! identifies the origin as Owncast with `ActivityPub` enabled. Normal follows,
//! inbox delivery and Note identity remain completely generic.

use plamenu_ap::acct::Acct;
use plamenu_ap::activity::one_or_many;
use plamenu_db::account::{self, Account};
use plamenu_db::media::{self, Media, NewRemoteMedia};
use plamenu_db::remote_stream_source::{self, RemoteStreamSource};
use plamenu_db::status;
use plamenu_federation::ResolvedAcct;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::error::ApiError;
use crate::state::AppState;

const NODEINFO_REL: &str = "http://nodeinfo.diaspora.software/ns/schema/2.0";
/// `Owncast` delays its go-live Note by two minutes and suppresses another one
/// for quick reconnects. This window associates that one Note with only its
/// current/reconnected session, never a later broadcast hours or days away.
const NOTE_SESSION_WINDOW: time::Duration = time::Duration::minutes(10);

#[derive(Debug, Deserialize)]
struct NodeInfoDirectory {
    #[serde(default)]
    links: Vec<NodeInfoLink>,
}

#[derive(Debug, Deserialize)]
struct NodeInfoLink {
    rel: String,
    href: String,
}

#[derive(Debug, Deserialize)]
struct NodeInfo {
    software: NodeInfoSoftware,
    #[serde(default)]
    protocols: Vec<String>,
    #[serde(default)]
    metadata: NodeInfoMetadata,
}

#[derive(Debug, Deserialize)]
struct NodeInfoSoftware {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct NodeInfoMetadata {
    #[serde(default)]
    federation: NodeInfoFederation,
}

#[derive(Debug, Default, Deserialize)]
struct NodeInfoFederation {
    #[serde(default)]
    username: String,
}

#[derive(Debug, Deserialize)]
struct PublicConfig {
    #[serde(default)]
    federation: PublicFederationConfig,
}

#[derive(Debug, Default, Deserialize)]
struct PublicFederationConfig {
    #[serde(default)]
    account: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicStatus {
    online: bool,
    #[serde(default)]
    stream_title: String,
    last_connect_time: Option<String>,
}

#[derive(Debug)]
struct InspectedOrigin {
    homepage_url: String,
    status_url: String,
    username: String,
}

#[derive(Debug)]
struct CurrentStatus {
    online: bool,
    title: Option<String>,
    connected_at: Option<OffsetDateTime>,
}

fn origin_root(raw: &str) -> Option<String> {
    let mut url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        return None;
    }
    // Plain HTTP is a federation URL only for hidden services; keep discovery
    // under the exact same transport policy as every other remote resource.
    if !plamenu_federation::is_federation_url(raw) {
        return None;
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

fn same_origin(a: &str, b: &str) -> bool {
    let (Ok(a), Ok(b)) = (Url::parse(a), Url::parse(b)) else {
        return false;
    };
    a.origin() == b.origin()
}

fn root_endpoint(root: &str, path: &str) -> Option<String> {
    Url::parse(root).ok()?.join(path).ok().map(Into::into)
}

fn acct_domain(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

async fn inspect_origin(state: &AppState, raw: &str) -> Option<InspectedOrigin> {
    let homepage_url = origin_root(raw)?;
    let directory_url = root_endpoint(&homepage_url, ".well-known/nodeinfo")?;
    let directory = state
        .federation
        .fetch_page(&directory_url, "application/json")
        .await
        .ok()?;
    let directory: NodeInfoDirectory = serde_json::from_str(&directory.body).ok()?;
    let nodeinfo_url = directory
        .links
        .into_iter()
        .find(|link| link.rel == NODEINFO_REL)
        .map(|link| link.href)?;
    if !same_origin(&homepage_url, &nodeinfo_url) {
        return None;
    }
    let document = state
        .federation
        .fetch_page(&nodeinfo_url, "application/json")
        .await
        .ok()?;
    let nodeinfo: NodeInfo = serde_json::from_str(&document.body).ok()?;
    if !nodeinfo.software.name.eq_ignore_ascii_case("owncast")
        || !nodeinfo
            .protocols
            .iter()
            .any(|protocol| protocol.eq_ignore_ascii_case("activitypub"))
    {
        return None;
    }

    // v0.3 exposes this directly in NodeInfo. v0.2.5 (still widely deployed)
    // exposes the same public identity in `/api/config` instead.
    let username = if nodeinfo.metadata.federation.username.is_empty() {
        let config_url = root_endpoint(&homepage_url, "api/config")?;
        let page = state
            .federation
            .fetch_page(&config_url, "application/json")
            .await
            .ok()?;
        let config: PublicConfig = serde_json::from_str(&page.body).ok()?;
        let (username, domain) = config.federation.account.split_once('@')?;
        let root = Url::parse(&homepage_url).ok()?;
        if !acct_domain(&root).is_some_and(|host| host.eq_ignore_ascii_case(domain)) {
            return None;
        }
        username.to_owned()
    } else {
        nodeinfo.metadata.federation.username
    };
    if username.is_empty() {
        return None;
    }
    Some(InspectedOrigin {
        status_url: root_endpoint(&homepage_url, "api/status")?,
        homepage_url,
        username,
    })
}

/// Learns `Owncast` playback only from an HLS-advertising `WebFinger` resolution
/// whose origin independently identifies itself as `Owncast` through `NodeInfo`.
/// Network/detection failures are intentionally a no-op; account resolution
/// must keep working for every other implementation.
pub async fn learn_from_resolution(
    state: &AppState,
    account: &Account,
    resolved: &ResolvedAcct,
) -> Result<Option<RemoteStreamSource>, plamenu_db::DbError> {
    if account.actor_type.as_deref() != Some("Service") {
        return Ok(None);
    }
    let Some(hls_master_url) = resolved.hls_stream_url.as_deref() else {
        return Ok(None);
    };
    let Some(inspected) = inspect_origin(state, hls_master_url).await else {
        return Ok(None);
    };
    if !same_origin(&inspected.homepage_url, hls_master_url)
        || !account
            .uri
            .as_deref()
            .is_some_and(|uri| same_origin(&inspected.homepage_url, uri))
        || !resolved
            .acct
            .username()
            .eq_ignore_ascii_case(&inspected.username)
    {
        return Ok(None);
    }
    Ok(Some(
        remote_stream_source::upsert_owncast(
            &state.pool,
            account.id,
            &inspected.homepage_url,
            &inspected.status_url,
            hls_master_url,
        )
        .await?,
    ))
}

/// Resolves an exact `Owncast` homepage URL to its federated account. This is a
/// narrow positive fallback for the root page, which has no `ActivityPub`
/// alternate of its own; arbitrary HTML pages never enter this path.
pub async fn discover_homepage(state: &AppState, raw: &str) -> Result<Option<Account>, ApiError> {
    let Some(root) = origin_root(raw) else {
        return Ok(None);
    };
    let parsed = Url::parse(raw).ok();
    if parsed
        .as_ref()
        .is_none_or(|url| url.path() != "/" || url.query().is_some() || url.fragment().is_some())
    {
        return Ok(None);
    }
    if let Some(source) = remote_stream_source::find_by_homepage(&state.pool, &root).await? {
        return Ok(account::find_by_id(&state.pool, source.account_id).await?);
    }
    let Some(inspected) = inspect_origin(state, &root).await else {
        return Ok(None);
    };
    let domain = Url::parse(&root).ok().as_ref().and_then(acct_domain);
    let Some(domain) = domain else {
        return Ok(None);
    };
    let Ok(acct) = Acct::new(&inspected.username, &domain) else {
        return Ok(None);
    };
    let Ok(resolved) = state.federation.resolve_acct(&acct).await else {
        return Ok(None);
    };
    let candidates =
        crate::remote::resolve_remote_accounts(state, &acct, account::ActorClass::PersonLike)
            .await?;
    for candidate in candidates {
        if candidate.actor_type.as_deref() != Some("Service")
            || !candidate
                .uri
                .as_deref()
                .is_some_and(|uri| same_origin(&root, uri))
        {
            continue;
        }
        if learn_from_resolution(state, &candidate, &resolved)
            .await?
            .is_some()
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

async fn fetch_status(
    state: &AppState,
    source: &RemoteStreamSource,
) -> Result<CurrentStatus, ApiError> {
    let page = state
        .federation
        .fetch_page(&source.status_url, "application/json")
        .await
        .map_err(|error| ApiError::BadGateway(error.to_string()))?;
    let status: PublicStatus = serde_json::from_str(&page.body)
        .map_err(|error| ApiError::BadGateway(error.to_string()))?;
    Ok(CurrentStatus {
        online: status.online,
        title: (!status.stream_title.is_empty()).then_some(status.stream_title),
        connected_at: status
            .last_connect_time
            .as_deref()
            .and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok()),
    })
}

fn session_matches(note_at: OffsetDateTime, connected_at: OffsetDateTime) -> bool {
    (note_at - connected_at).abs() <= NOTE_SESSION_WINDOW
}

fn preview_url(object: &Value, homepage_url: &str) -> Option<String> {
    for attachment in one_or_many(object.get("attachment")) {
        if attachment.get("type").and_then(Value::as_str) != Some("Image")
            || attachment.get("name").and_then(Value::as_str) != Some("Live stream preview")
        {
            continue;
        }
        let Some(url) = attachment.get("url").and_then(|value| {
            value.as_str().or_else(|| {
                one_or_many(Some(value)).iter().find_map(|entry| {
                    entry
                        .as_str()
                        .or_else(|| entry.get("href").and_then(Value::as_str))
                })
            })
        }) else {
            continue;
        };
        let Ok(parsed) = Url::parse(url) else {
            continue;
        };
        if same_origin(homepage_url, url)
            && matches!(parsed.path(), "/preview.gif" | "/thumbnail.jpg")
        {
            return Some(url.to_owned());
        }
    }
    None
}

/// Replaces a positively identified current `Owncast` go-live Note's cosmetic
/// preview image with the playable HLS live attachment it represents.
pub async fn maybe_store_live_note(
    state: &AppState,
    stored: &status::Status,
    object: &Value,
) -> Result<bool, ApiError> {
    let Some(source) =
        remote_stream_source::find_by_account(&state.pool, stored.account_id).await?
    else {
        return Ok(false);
    };
    let Some(preview) = preview_url(object, &source.homepage_url) else {
        return Ok(false);
    };
    let Ok(current) = fetch_status(state, &source).await else {
        // The Note is still valid federation content when the auxiliary
        // status endpoint is transiently unavailable; fall back to its normal
        // image attachment instead of rejecting inbox delivery.
        return Ok(false);
    };
    if !current.online
        || !current
            .connected_at
            .is_some_and(|connected| session_matches(stored.created_at, connected))
    {
        return Ok(false);
    }
    remote_stream_source::end_prior_status_media(&state.pool, stored.account_id, stored.id).await?;
    media::create_remote(
        &state.pool,
        NewRemoteMedia {
            account_id: stored.account_id,
            status_id: stored.id,
            // The federated Note/homepage is the attachment identity; the HLS
            // playlist remains in its dedicated field and is never treated as
            // a downloadable immutable file.
            remote_url: &source.homepage_url,
            content_type: "application/x-mpegURL",
            description: current.title.as_deref(),
            thumbnail_remote_url: Some(&preview),
            download_on_demand: true,
            hls_master_url: Some(&source.hls_master_url),
            live_state: Some("live"),
            live_permanent: true,
            ..Default::default()
        },
    )
    .await?;
    Ok(true)
}

/// Refreshes an account-level or Note-level `Owncast` media row from the public
/// status endpoint. Note players may re-arm only inside their short reconnect
/// window; the account-level player always represents the latest session.
pub async fn refresh_media(state: &AppState, item: &Media) -> Result<bool, ApiError> {
    let Some(source) = remote_stream_source::find_by_account(&state.pool, item.account_id).await?
    else {
        return Ok(false);
    };
    let current = fetch_status(state, &source).await?;
    let (live_state, permanent) = if let Some(status_id) = item.status_id {
        let Some(note) = status::find_by_id(&state.pool, status_id).await? else {
            return Ok(false);
        };
        let current_session = current.online
            && current
                .connected_at
                .is_some_and(|connected| session_matches(note.created_at, connected));
        let reconnectable =
            (OffsetDateTime::now_utc() - note.created_at).abs() <= NOTE_SESSION_WINDOW;
        if current_session {
            ("live", true)
        } else {
            ("ended", reconnectable)
        }
    } else {
        (if current.online { "live" } else { "ended" }, true)
    };
    Ok(remote_stream_source::update_media_state(
        &state.pool,
        item.id,
        live_state,
        permanent,
        current.title.as_deref(),
        (item.status_id.is_none())
            .then(|| format!("{}thumbnail.jpg", source.homepage_url))
            .as_deref(),
        &source.hls_master_url,
    )
    .await?)
}

/// The live account-level player for an `Owncast` profile, if one is on air and
/// no federated go-live Note already supplies the same player in that feed.
pub async fn profile_live_media(
    state: &AppState,
    account_id: i64,
) -> Result<Option<Media>, ApiError> {
    let Some(source) = remote_stream_source::find_by_account(&state.pool, account_id).await? else {
        return Ok(None);
    };
    let Some(item) = remote_stream_source::media(&state.pool, &source).await? else {
        return Ok(None);
    };
    let _ = crate::live_refresh::refresh_for_media(state, item.id).await;
    let Some(refreshed) = remote_stream_source::media(&state.pool, &source).await? else {
        return Ok(None);
    };
    if refreshed.live_state.as_deref() != Some("live")
        || remote_stream_source::has_live_status_media(&state.pool, account_id).await?
    {
        return Ok(None);
    }
    Ok(Some(refreshed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_are_canonical_and_preview_is_strict() {
        assert_eq!(
            origin_root("https://stream.example/hls/stream.m3u8"),
            Some("https://stream.example/".into())
        );
        assert!(origin_root("file:///tmp/stream").is_none());
        let note = serde_json::json!({
            "attachment": [{
                "type": "Image",
                "name": "Live stream preview",
                "url": "https://stream.example/preview.gif?us=one"
            }]
        });
        assert_eq!(
            preview_url(&note, "https://stream.example/").as_deref(),
            Some("https://stream.example/preview.gif?us=one")
        );
        assert!(preview_url(&note, "https://other.example/").is_none());

        let malformed_then_valid = serde_json::json!({
            "attachment": [
                {"type": "Image", "name": "Live stream preview", "url": "not a URL"},
                {"type": "Image", "name": "Live stream preview", "url": "https://stream.example/thumbnail.jpg"}
            ]
        });
        assert_eq!(
            preview_url(&malformed_then_valid, "https://stream.example/").as_deref(),
            Some("https://stream.example/thumbnail.jpg")
        );
        assert_eq!(
            acct_domain(&Url::parse("https://stream.example:8443/").unwrap()).as_deref(),
            Some("stream.example:8443")
        );
    }

    #[test]
    fn only_nearby_sessions_reuse_a_note() {
        let note = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2);
        assert!(session_matches(note, note - time::Duration::minutes(2)));
        assert!(session_matches(note, note + time::Duration::minutes(5)));
        assert!(!session_matches(note, note + time::Duration::hours(1)));
    }
}
