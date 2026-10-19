//! Explicit, bounded searches of named source origins. Queries are transient;
//! receipts contain a digest and canonical identities, never remote snippets.
use super::{browse, communities, directory, outbox, profiles, transport};
use crate::{AppState, error::ApiError};
use plamenu_db::{account_domain_block, discovery_store as db};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};
use time::OffsetDateTime;

pub fn term(raw: &str) -> Result<&str, ApiError> {
    let value = raw.trim();
    if value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("Invalid discovery query".into()));
    }
    Ok(value)
}
#[must_use]
pub fn digest(kind: &str, query: &str) -> String {
    crate::instance_policy::sha256_hex(&format!("{kind}\0{query}"))
}

pub async fn enabled(state: &AppState) -> Result<bool, ApiError> {
    let s = db::settings(&state.pool).await?;
    Ok(s.enabled
        && !s.paused
        && !s
            .disabled_mechanisms
            .iter()
            .any(|m| ["wider_search", "metadata_cache", "profile_listing"].contains(&m.as_str())))
}

pub async fn sources(
    state: &AppState,
    viewer: i64,
    kind: &str,
    only: Option<i64>,
) -> Result<Vec<db::Source>, ApiError> {
    let mut sources = Vec::new();
    if !enabled(state).await? {
        return Ok(sources);
    }
    let domains = account_domain_block::all_domains(&state.pool, viewer).await?;
    let capability = if kind == "community" {
        "community_directory"
    } else {
        "directory"
    };
    let mut origins = HashSet::new();
    for source in db::browse_sources(&state.pool, 0).await? {
        if only.is_some_and(|id| id != source.id)
            || !source.capabilities.iter().any(|c| c == capability)
            || source
                .cooldown_until
                .is_some_and(|v| v > OffsetDateTime::now_utc())
            || !browse::source_visible(state, &source, &domains).await?
            || !db::search_source_ready(&state.pool, source.id, kind).await?
            || !origins.insert(source.origin.clone())
        {
            continue;
        }
        sources.push(source);
        if sources.len() == 3 {
            break;
        }
    }
    Ok(sources)
}

pub fn query_url(origin: &str, kind: &str, query: &str) -> Result<String, FederationError> {
    let mut url = url::Url::parse(origin).map_err(|_| invalid())?;
    url.set_query(None);
    url.set_fragment(None);
    if kind == "profile" {
        url.set_path("/api/v2/search");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("type", "accounts")
            .append_pair("resolve", "false")
            .append_pair("limit", "3");
    } else if kind == "community" {
        url.set_path("/api/v3/search");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("type_", "Communities")
            .append_pair("listing_type", "Local")
            .append_pair("sort", "TopAll")
            .append_pair("page", "1")
            .append_pair("limit", "3");
    } else {
        return Err(invalid());
    }
    Ok(url.to_string())
}
fn invalid() -> FederationError {
    FederationError::InvalidActor("invalid public search response".into())
}

pub fn hints(
    document: &Value,
    kind: &str,
    origin: &str,
) -> Result<Vec<directory::Hint>, FederationError> {
    let rows = document
        .get(if kind == "profile" {
            "accounts"
        } else {
            "communities"
        })
        .and_then(Value::as_array)
        .filter(|a| a.len() <= 3)
        .ok_or_else(invalid)?;
    if kind == "profile" {
        Ok(directory::parse_page(&Value::Array(rows.clone()), origin)?.0)
    } else {
        Ok(communities::parse_page(document, origin)?
            .0
            .into_iter()
            .map(|uri| directory::Hint {
                activity: 1,
                uri,
                canonical: true,
            })
            .collect())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceResult {
    pub source: i64,
    pub outcome: String,
    pub entries: Vec<i64>,
}

fn failure(error: &FederationError) -> (&'static str, i32) {
    match error {
        FederationError::Status(401 | 403) => ("authentication", 86400),
        FederationError::Status(404 | 405 | 410 | 501) => ("unsupported", 86400),
        FederationError::RateLimited { retry_after_secs } => (
            "rate_limited",
            i32::try_from(retry_after_secs.unwrap_or(60).clamp(1, 86400)).unwrap_or(86400),
        ),
        FederationError::InvalidActor(_) | FederationError::DocumentTooLarge(_) => {
            ("unavailable", 3600)
        }
        _ => ("unavailable", 60),
    }
}

async fn cancelled(state: &AppState, lease: &db::Lease) {
    loop {
        if !(transport::Budget { state, lease })
            .allowed()
            .await
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn remote_hints(
    state: &AppState,
    run: &db::SearchRun,
    source: &db::Source,
    query: &str,
) -> Result<Option<Vec<directory::Hint>>, ApiError> {
    let Some(lease) =
        db::search_lease(&state.pool, run, source.id, &source.endpoint, false).await?
    else {
        return Ok(None);
    };
    let url = query_url(&source.origin, &run.kind, query)
        .map_err(|_| ApiError::BadRequest("Unsupported search kind".into()))?;
    let result = tokio::select! {
        biased;
        ()=cancelled(state,&lease)=>Err(FederationError::DiscoveryStale),
        response=transport::fetch(state,&lease,&url,DiscoveryFormat::LocalSearchJson)=>response.and_then(|r|hints(&r.document,&run.kind,&source.origin)),
    };
    db::close_search_lease(&state.pool, &lease).await?;
    match result {
        Ok(hints) => {
            db::search_source_result(&state.pool, source.id, &run.kind, "ready", 0).await?;
            Ok(Some(hints))
        }
        Err(FederationError::DiscoveryStale | FederationError::DiscoveryDeferred(_)) => Ok(None),
        Err(error) => {
            let (outcome, seconds) = failure(&error);
            db::search_source_result(&state.pool, source.id, &run.kind, outcome, seconds).await?;
            Ok(None)
        }
    }
}

async fn verification_backoff(
    state: &AppState,
    run: &db::SearchRun,
    lease: &db::Lease,
    error: &FederationError,
) -> Result<(), ApiError> {
    if matches!(error, FederationError::RateLimited { .. }) {
        let (outcome, seconds) = failure(error);
        // Canonical AP shares its origin's ordinary federation cooldown.
        // The public search API's own failure cache remains independent.
        db::fail_job(&state.pool, lease, seconds, outcome, true).await?;
        db::search_source_result(&state.pool, lease.source_id, &run.kind, outcome, seconds).await?;
    }
    Ok(())
}

/// Older public account APIs only expose a human URL. Resolve its identity on
/// the same chosen origin, then independently fetch that canonical actor. No
/// alias work survives the original deadline or enters the background queue.
async fn resolve_alias(
    state: &AppState,
    run: &db::SearchRun,
    source: &db::Source,
    uri: &str,
) -> Result<Option<String>, ApiError> {
    if !outbox::same_origin(uri, &source.origin) {
        return Ok(None);
    }
    let Some(lease) = db::search_lease(&state.pool, run, source.id, uri, true).await? else {
        return Ok(None);
    };
    let response = tokio::select! {
        biased;
        ()=cancelled(state,&lease)=>Err(FederationError::DiscoveryStale),
        response=transport::fetch(state,&lease,uri,DiscoveryFormat::ActivityPub)=>response,
    };
    if let Err(error) = &response {
        verification_backoff(state, run, &lease, error).await?;
    }
    db::close_search_lease(&state.pool, &lease).await?;
    Ok(response.ok().and_then(|r| {
        r.document
            .get("id")
            .and_then(Value::as_str)
            .filter(|uri| uri.len() <= 2048 && outbox::same_origin(uri, &source.origin))
            .map(str::to_owned)
    }))
}

async fn verify(
    state: &AppState,
    run: &db::SearchRun,
    source: &db::Source,
    hint: &directory::Hint,
) -> Result<Option<i64>, ApiError> {
    let canonical = if hint.canonical {
        Some(hint.uri.clone())
    } else {
        resolve_alias(state, run, source, &hint.uri).await?
    };
    let Some(uri) = canonical.as_deref() else {
        return Ok(None);
    };
    if !outbox::same_origin(uri, &source.origin) {
        return Ok(None);
    }
    // No work for a result already hidden by this member, even before fetching.
    if let Some(entry) = db::entry(&state.pool, uri).await?
        && db::viewer_hides(&state.pool, run.account_id, entry.id).await?
    {
        return Ok(None);
    }
    let Some(lease) = db::search_lease(&state.pool, run, source.id, uri, true).await? else {
        return Ok(None);
    };
    let outcome = tokio::select! {
        biased;
        ()=cancelled(state,&lease)=>Err(FederationError::DiscoveryStale),
        result=async {if run.kind=="community" {communities::fetch(state,&lease).await} else {profiles::fetch(state,&lease).await}}=>result,
    };
    if let Err(error) = &outcome {
        verification_backoff(state, run, &lease, error).await?;
    }
    // Also cancels a deferral: interactive verification has no background retry.
    db::close_search_lease(&state.pool, &lease).await?;
    // Committing canonical evidence also releases its lease. A simultaneous
    // cancellation check may observe that release before the fetch returns;
    // keep any freshly committed, currently eligible result in the receipt.
    let Some(entry) = db::entry(&state.pool, uri).await? else {
        return Ok(None);
    };
    if entry.kind != run.kind || entry.verified_at < run.created_at {
        return Ok(None);
    }
    Ok(browse::eligible(state, run.account_id, entry.id)
        .await?
        .map(|e| e.id))
}

pub async fn search(
    state: &AppState,
    viewer: i64,
    kind: &str,
    query: &str,
    selected: &[i64],
) -> Result<db::SearchStart, ApiError> {
    let query = term(query)?;
    if query.is_empty()
        || !["profile", "community"].contains(&kind)
        || selected.is_empty()
        || selected.len() > 3
    {
        return Err(ApiError::BadRequest(
            "Select up to three servers and enter a query".into(),
        ));
    }
    let mut chosen = Vec::new();
    let mut origins = HashSet::new();
    for id in selected {
        if let Some(source) = sources(state, viewer, kind, Some(*id))
            .await?
            .into_iter()
            .next()
            && origins.insert(source.origin.clone())
        {
            chosen.push(source);
        }
    }
    if chosen.is_empty() {
        return Ok(db::SearchStart::Disabled);
    }
    let mut ids: Vec<_> = chosen.iter().map(|s| s.id).collect();
    ids.sort_unstable();
    let start = db::start_search(&state.pool, viewer, &digest(kind, query), kind, &ids).await?;
    let db::SearchStart::Started(run) = start else {
        return Ok(start);
    };
    let mut results: Vec<SourceResult> = chosen
        .iter()
        .map(|s| SourceResult {
            source: s.id,
            outcome: "deadline".into(),
            entries: Vec::new(),
        })
        .collect();
    let remaining = Duration::try_from(run.deadline - OffsetDateTime::now_utc())
        .unwrap_or(Duration::ZERO)
        .min(Duration::from_secs(15));
    let _ = tokio::time::timeout(remaining, async {
        for (source, result) in chosen.iter().zip(&mut results) {
            let Some(hints) = remote_hints(state, &run, source, query).await? else {
                result.outcome = "unavailable".into();
                continue;
            };
            for hint in hints {
                if let Some(id) = verify(state, &run, source, &hint).await? {
                    result.entries.push(id);
                }
            }
            if OffsetDateTime::now_utc() < run.deadline {
                result.outcome = if enabled(state).await?
                    && db::search_source_ready(&state.pool, source.id, &run.kind).await?
                {
                    "checked"
                } else {
                    "unavailable"
                }
                .into();
            }
        }
        Ok::<(), ApiError>(())
    })
    .await;
    db::finish_search(
        &state.pool,
        run.id,
        &serde_json::to_value(&results)
            .map_err(|_| ApiError::Internal("Invalid search results".into()))?,
    )
    .await?;
    Ok(db::SearchStart::Started(run))
}
