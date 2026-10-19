//! Bounded `PeerTube` catalogue discovery. All text comes from canonical objects;
//! the public REST record establishes publication/access and actor identities.
use super::{outbox, profiles, transport};
use crate::AppState;
use plamenu_db::discovery_store::{self as db, Lease, VideoHint};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use url::Url;

mod browse;
mod verify;
pub use browse::{eligible, page};
pub use verify::fetch;
pub const PATH: &str = "/api/v1/videos";
fn invalid() -> FederationError {
    FederationError::InvalidActor("invalid public video catalogue".into())
}
fn db_error(e: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(e.to_string())
}
#[must_use]
pub fn is_endpoint(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|u| {
        u.path() == PATH
            && u.query().is_none()
            && u.fragment().is_none()
            && outbox::same_origin(raw, raw)
    })
}
pub fn endpoint(raw: &str) -> Result<String, FederationError> {
    if is_endpoint(raw) {
        Ok(raw.to_owned())
    } else {
        super::communities::source_endpoint(raw)
    }
}
pub fn page_uri(source: &str) -> Result<String, FederationError> {
    sampling_uri(source, db::SamplingPlan::default())
}
pub fn sampling_uri(source: &str, plan: db::SamplingPlan) -> Result<String, FederationError> {
    if !(0..400).contains(&plan.offset) || plan.offset % 8 != 0 {
        return Err(invalid());
    }
    let mut url = Url::parse(source).map_err(|_| invalid())?;
    url.set_path(PATH);
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("isLocal", "true")
        .append_pair("isLive", "false")
        .append_pair("nsfw", "both")
        .append_pair("sort", "-publishedAt")
        .append_pair("count", "8")
        .append_pair("start", &plan.offset.to_string());
    Ok(url.into())
}
#[must_use]
pub fn public_record(v: &Value, detail: bool) -> bool {
    v["isLocal"] == true
        && v["privacy"]["id"] == 1
        && v["isLive"] == false
        && v["nsfw"].is_boolean()
        && [
            "blacklisted",
            "blockedOwner",
            "blockedServer",
            "requiresPassword",
        ]
        .iter()
        .all(|k| v.get(*k).is_none_or(|v| v == &Value::Bool(false)))
        && (!detail || v["state"]["id"] == 1)
}
fn record_language(v: &Value) -> Option<&str> {
    v.pointer("/language/id")
        .and_then(Value::as_str)
        .filter(|code| crate::actions::is_language_code(code))
}

fn record_hint(v: &Value, origin: &str) -> Option<(String, VideoHint)> {
    let uuid = v["uuid"].as_str()?;
    if uuid.len() != 36
        || !uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
    {
        return None;
    }
    let uri = v["url"].as_str()?;
    let expected = format!("{origin}/videos/watch/{uuid}");
    let owner = v["account"]["url"].as_str()?;
    let channel = v["channel"]["url"].as_str()?;
    if uri != expected
        || !outbox::same_origin(owner, origin)
        || !outbox::same_origin(channel, origin)
        || owner == channel
    {
        return None;
    }
    Some((
        uri.to_owned(),
        VideoHint {
            uuid: uuid.into(),
            owner_uri: owner.into(),
            channel_uri: channel.into(),
        },
    ))
}
pub fn parse_page(
    document: &Value,
    origin: &str,
) -> Result<Vec<(String, VideoHint)>, FederationError> {
    let rows = document["data"]
        .as_array()
        .filter(|r| r.len() <= 8)
        .ok_or_else(invalid)?;
    let mut hints = Vec::new();
    for row in rows {
        if !row.is_object() {
            return Err(invalid());
        }
        if public_record(row, false)
            && let Some(hint) = record_hint(row, origin)
            && !hints.iter().any(|h: &(String, VideoHint)| h.0 == hint.0)
        {
            hints.push(hint);
        }
    }
    Ok(hints)
}
pub async fn fetch_page(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "video_catalogue" || m == "video_indexing")
    {
        return Err(FederationError::DiscoveryDeferred("videos_disabled"));
    }
    let plan = db::sampling_plan(&state.pool, lease)
        .await
        .map_err(|e| db_error(&e))?;
    let response = transport::fetch(
        state,
        lease,
        &sampling_uri(&lease.target_uri, plan)?,
        DiscoveryFormat::Json,
    )
    .await?;
    let origin = Url::parse(&lease.target_uri)
        .map_err(|_| invalid())?
        .origin()
        .ascii_serialization();
    let versions: Vec<_> = response
        .document
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row.get("url")?.as_str()?.to_owned(),
                row.get("updatedAt")?.as_str()?.to_owned(),
            ))
        })
        .take(8)
        .collect();
    db::observe_versions(&state.pool, lease, &versions)
        .await
        .map_err(|e| db_error(&e))?;
    let mut hints = parse_page(&response.document, &origin)?;
    // The already fetched, same-origin public page can fill missing display
    // metadata on known videos without another per-video fetch or consent change.
    let languages: Vec<_> = response.document["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            if !public_record(row, false) {
                return None;
            }
            let (uri, _) = record_hint(row, &origin)?;
            Some((uri, record_language(row)?.to_owned()))
        })
        .collect();
    db::save_catalogue_languages(&state.pool, lease, &languages)
        .await
        .map_err(|e| db_error(&e))?;

    let full = response.document["data"]
        .as_array()
        .is_some_and(|a| a.len() == 8);
    hints.sort_by_key(|(uri, _)| super::sampling::rank(uri, plan.seed, plan.round));
    db::finish_video_sample(&state.pool, lease, &hints, full)
        .await
        .map_err(|e| db_error(&e))?;
    Ok(())
}
/// Probe only after an unsupported directory, never after authentication,
/// throttling or malformed data. Previously established video sources use their
/// own adapter even if their admitted address began as an instance origin.
pub async fn inspect(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let source = db::source(&state.pool, lease.source_id)
        .await
        .map_err(|e| db_error(&e))?;
    if is_endpoint(&lease.target_uri)
        || source.is_some_and(|s| s.capabilities.iter().any(|c| c == "video_catalogue"))
    {
        return fetch_page(state, lease).await;
    }
    match super::communities::inspect(state, lease).await {
        Err(FederationError::Status(404 | 405 | 501)) => fetch_page(state, lease).await,
        result => result,
    }
}

/// Negative public-visibility evidence learned during ordinary authenticated
/// delivery clears discovery; a public Create does not undo a prior verification.
pub async fn public_delivery(state: &AppState, v: &Value) -> Result<bool, plamenu_db::DbError> {
    use plamenu_ap::{
        activity::{id_of, one_or_many},
        discovery::{DiscoverySignals, Signal},
    };
    if v["type"] != "Video" {
        return Ok(false);
    }
    if let Some(uri) = id_of(v)
        && let Some(entry) = db::entry(&state.pool, uri).await?
        && let Some(details) = db::video_details(&state.pool, entry.id).await?
        && !verify::public_video(
            v,
            &VideoHint {
                uuid: details.uuid,
                owner_uri: details.owner_uri,
                channel_uri: details.channel_uri,
            },
        )
    {
        return Ok(false);
    }
    let signals = DiscoverySignals::from_document(v);
    Ok(one_or_many(v.get("to"))
        .iter()
        .any(|v| id_of(v) == Some("https://www.w3.org/ns/activitystreams#Public"))
        && ["bto", "bcc"]
            .iter()
            .all(|k| one_or_many(v.get(*k)).is_empty())
        && v.get("visibility")
            .is_none_or(|v| v.as_str() == Some("public"))
        && ["localOnly", "suspended", "isLiveBroadcast"]
            .iter()
            .all(|k| v.get(*k).is_none_or(|v| v == &Value::Bool(false)))
        && v.get("state").is_none_or(|v| v == &Value::from(1))
        && !matches!(signals.indexable, Signal::Deny | Signal::Invalid)
        && !matches!(signals.discoverable, Signal::Deny | Signal::Invalid))
}
