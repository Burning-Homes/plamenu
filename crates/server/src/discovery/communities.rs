//! Lemmy v3 public community directories. Entries are hints, never actor facts.
use super::{directory, outbox, transport};
use crate::AppState;
use plamenu_db::discovery_store::{self as db, Lease, ProfileHint};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use url::Url;

mod profile;
pub use profile::{fetch, public_community};
pub const PATH: &str = "/api/v3/community/list";

fn invalid() -> FederationError {
    FederationError::InvalidActor("invalid public community directory".into())
}
pub fn source_endpoint(raw: &str) -> Result<String, FederationError> {
    if is_endpoint(raw) {
        Ok(raw.to_owned())
    } else {
        directory::endpoint(raw)
    }
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
pub fn page_uri(source: &str, second: bool) -> Result<String, FederationError> {
    sampling_uri(
        source,
        db::SamplingPlan {
            offset: if second { 20 } else { 0 },
            ..Default::default()
        },
    )
}
pub fn sampling_uri(source: &str, plan: db::SamplingPlan) -> Result<String, FederationError> {
    if !(0..400).contains(&plan.offset) || plan.offset % 20 != 0 {
        return Err(invalid());
    }
    let mut url = Url::parse(source).map_err(|_| invalid())?;
    url.set_path(PATH);
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("type_", "Local")
        // Lemmy v3 maps Active to hot_rank. TopMonth counts active participants,
        // avoiding that ranking's strong preference for newly created groups.
        .append_pair("sort", if plan.newcomers() { "New" } else { "TopMonth" })
        .append_pair("limit", "20")
        .append_pair("page", &(plan.offset / 20 + 1).to_string())
        .append_pair("show_nsfw", "true");
    Ok(url.to_string())
}

/// Public directory inclusion is insufficient: require the same declarations
/// again from the origin's individual community response before retaining text.
#[must_use]
pub fn public_record(record: &Value) -> bool {
    record.get("local") == Some(&Value::Bool(true))
        && ["deleted", "removed", "hidden"]
            .iter()
            .all(|key| record.get(*key) == Some(&Value::Bool(false)))
        && matches!(
            record.get("visibility").and_then(Value::as_str),
            Some("Public" | "public")
        )
        && record.get("nsfw").is_some_and(Value::is_boolean)
}
pub fn parse_page(document: &Value, origin: &str) -> Result<(Vec<String>, bool), FederationError> {
    let rows = document
        .get("communities")
        .and_then(Value::as_array)
        .filter(|r| r.len() <= 20)
        .ok_or_else(invalid)?;
    let mut hints = Vec::new();
    for row in rows {
        let record = row
            .get("community")
            .filter(|r| r.is_object())
            .ok_or_else(invalid)?;
        if public_record(record)
            && let Some(uri) = record.get("actor_id").and_then(Value::as_str)
            && outbox::same_origin(uri, origin)
            && !hints.iter().any(|known| known == uri)
        {
            hints.push(uri.to_owned());
        }
    }
    Ok((hints, rows.len() == 20))
}

pub async fn fetch_page(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "community_directory")
    {
        return Err(FederationError::DiscoveryDeferred(
            "community_directory_disabled",
        ));
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
    let (mut uris, full) = parse_page(&response.document, &lease.target_uri)?;
    let labels = response.document["communities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let record = &row["community"];
            let uri = record.get("actor_id")?.as_str()?;
            let username = record.get("name")?.as_str()?;
            uris.iter()
                .any(|u| u == uri)
                .then(|| (uri.to_owned(), username.to_owned()))
        })
        .collect::<Vec<_>>();
    db::save_catalogue_labels(&state.pool, lease, &labels)
        .await
        .map_err(|e| db_error(&e))?;
    uris.sort_by_key(|uri| super::sampling::rank(uri, plan.seed, plan.round));
    let hints: Vec<_> = uris
        .iter()
        .map(|uri| ProfileHint {
            uri,
            canonical: true,
        })
        .collect();
    db::finish_community_page(&state.pool, lease, &hints, full)
        .await
        .map_err(|e| db_error(&e))?;
    Ok(())
}

pub(super) fn db_error(error: &plamenu_db::DbError) -> FederationError {
    FederationError::Admission(error.to_string())
}

/// Existing origins probe one extra public capability after an unsupported
/// directory. Authentication failures, throttling and malformed data do not
/// trigger another adapter. Known community sources use their verified method.
pub async fn inspect(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let source = db::source(&state.pool, lease.source_id)
        .await
        .map_err(|e| db_error(&e))?;
    if is_endpoint(&lease.target_uri)
        || source.is_some_and(|s| s.capabilities.iter().any(|c| c == "community_directory"))
    {
        return fetch_page(state, lease).await;
    }
    match directory::fetch_page(state, lease).await {
        Err(error @ FederationError::Status(404 | 405 | 501)) => {
            let settings = db::settings(&state.pool).await.map_err(|e| db_error(&e))?;
            if settings
                .disabled_mechanisms
                .iter()
                .any(|m| m == "community_directory")
            {
                return Err(error);
            }
            fetch_page(state, lease).await
        }
        result => result,
    }
}
