//! Mastodon-compatible public local directory. Branding is never a capability.
use super::transport::{self, Budget};
use crate::AppState;
use plamenu_db::discovery_store::{self as db, Lease, ProfileHint};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use url::Url;

pub const PATH: &str = "/api/v1/directory";

pub fn endpoint(raw: &str) -> Result<String, FederationError> {
    let mut url = Url::parse(raw).map_err(|e| FederationError::InvalidUrl(e.to_string()))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !["/", PATH].contains(&url.path())
    {
        return Err(FederationError::InvalidUrl(
            "use an instance origin or its /api/v1/directory endpoint".into(),
        ));
    }
    url.set_path(PATH);
    Ok(url.to_string())
}

#[must_use]
pub fn is_endpoint(raw: &str) -> bool {
    endpoint(raw).is_ok_and(|endpoint| endpoint == raw)
}

pub fn page_uri(endpoint: &str, offset: u8) -> Result<String, FederationError> {
    sampling_uri(
        endpoint,
        db::SamplingPlan {
            offset: i32::from(offset),
            ..Default::default()
        },
    )
}

pub fn sampling_uri(endpoint: &str, plan: db::SamplingPlan) -> Result<String, FederationError> {
    if !is_endpoint(endpoint) || !(0..400).contains(&plan.offset) || plan.offset % 20 != 0 {
        return Err(FederationError::InvalidUrl("invalid directory page".into()));
    }
    let mut url = Url::parse(endpoint).map_err(|e| FederationError::InvalidUrl(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("local", "true")
        .append_pair("order", if plan.newcomers() { "new" } else { "active" })
        .append_pair("limit", "20")
        .append_pair("offset", &plan.offset.to_string());
    Ok(url.to_string())
}

pub struct Hint {
    pub uri: String,
    pub canonical: bool,
    pub activity: u8,
}

pub fn parse_page(document: &Value, origin: &str) -> Result<(Vec<Hint>, bool), FederationError> {
    let rows = document
        .as_array()
        .filter(|rows| rows.len() <= 20)
        .ok_or_else(|| {
            FederationError::InvalidActor(
                "directory must be an array of at most 20 accounts".into(),
            )
        })?;
    let base = Url::parse(origin).map_err(|e| FederationError::InvalidUrl(e.to_string()))?;
    let mut hints = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        if !row.is_object() {
            return Err(FederationError::InvalidActor(
                "invalid directory account".into(),
            ));
        }
        // Never manufacture consent from a directory hint. Its explicit refusal
        // can exclude a new hint, but cannot revoke somebody else's stored data.
        if row.get("discoverable") == Some(&Value::Bool(false))
            || row.get("suspended") == Some(&Value::Bool(true))
            || row.get("group") == Some(&Value::Bool(true))
        {
            continue;
        }
        let canonical = row.get("uri").and_then(Value::as_str);
        let Some(raw) = canonical.or_else(|| row.get("url").and_then(Value::as_str)) else {
            continue;
        };
        let Ok(url) = Url::parse(raw) else {
            continue;
        };
        if raw.len() > 2048
            || url.origin() != base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            continue;
        }
        if seen.insert(url.to_string()) {
            hints.push(Hint {
                uri: url.to_string(),
                canonical: canonical.is_some(),
                // A hint can prioritize a check, never establish eligibility.
                activity: if row.get("last_status_at").is_some_and(Value::is_string)
                    || row
                        .get("statuses_count")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n > 0)
                {
                    0
                } else if row.get("statuses_count").and_then(Value::as_u64) == Some(0) {
                    2
                } else {
                    1
                },
            });
        }
    }
    Ok((hints, rows.len() == 20))
}

pub async fn fetch_page(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let settings = db::settings(&state.pool)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    if settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "directory")
    {
        return Err(FederationError::DiscoveryDeferred("directory_disabled"));
    }
    let plan = db::sampling_plan(&state.pool, lease)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    let uri = sampling_uri(&lease.target_uri, plan)?;
    let response = transport::fetch(state, lease, &uri, DiscoveryFormat::Json).await?;
    let versions: Vec<_> = response
        .document
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            Some((
                row.get("uri")
                    .or_else(|| row.get("url"))?
                    .as_str()?
                    .to_owned(),
                row.get("updated_at")
                    .or_else(|| row.get("last_status_at"))?
                    .as_str()?
                    .to_owned(),
            ))
        })
        .take(20)
        .collect();
    db::observe_versions(&state.pool, lease, &versions)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    let (mut hints, full) = parse_page(&response.document, &lease.target_uri)?;
    let labels = response
        .document
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let uri = row.get("uri")?.as_str()?;
            let username = row.get("username")?.as_str()?;
            hints
                .iter()
                .any(|h| h.canonical && h.uri == uri)
                .then(|| (uri.to_owned(), username.to_owned()))
        })
        .collect::<Vec<_>>();
    db::save_catalogue_labels(&state.pool, lease, &labels)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    hints.sort_by_key(|h| {
        (
            if plan.newcomers() { 0 } else { h.activity },
            super::sampling::rank(&h.uri, plan.seed, plan.round),
        )
    });
    if !(Budget { state, lease }).allowed().await? {
        return Err(FederationError::DiscoveryStale);
    }
    let hints: Vec<_> = hints
        .iter()
        .map(|h| ProfileHint {
            uri: &h.uri,
            canonical: h.canonical,
        })
        .collect();
    db::finish_directory_page(&state.pool, lease, &hints, full)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    Ok(())
}
