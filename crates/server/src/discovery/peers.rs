//! Public peer addresses are hints, never authority for identity or consent.
use super::transport::{self, Budget};
use crate::AppState;
use plamenu_db::discovery_store::{self as db, Lease};
use plamenu_federation::{DiscoveryFormat, FederationError};
use serde_json::Value;
use url::Url;

pub fn parse_page(document: &Value, offset: i32) -> Result<(Vec<String>, i32), FederationError> {
    let rows = document
        .as_array()
        .filter(|rows| rows.len() <= 262_144)
        .ok_or_else(|| {
            FederationError::InvalidActor("peer list must contain at most 262144 addresses".into())
        })?;
    let start = usize::try_from(offset).unwrap_or_default();
    let start = if start < rows.len() { start } else { 0 };
    let mut seen = std::collections::BTreeSet::new();
    let mut hints = Vec::new();
    let mut next = start;
    let stride = (rows.len() / 3).max(1);
    // A coprime stride traverses each observed row before repeating.
    let mut step = stride;
    while gcd(step, rows.len().max(1)) != 1 {
        step += 1;
    }
    for position in start..(start + 256).min(rows.len()) {
        let row = &rows[position * step % rows.len()];
        next += 1;
        let Some(host) = row.as_str() else {
            continue;
        };
        if host.len() > 253
            || host.is_empty()
            || host
                .chars()
                .any(|c| c.is_whitespace() || "/@?#\\:".contains(c))
        {
            continue;
        }
        let scheme = if host
            .rsplit_once('.')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("onion"))
        {
            "http"
        } else {
            "https"
        };
        let Ok(url) = Url::parse(&format!("{scheme}://{host}/api/v1/directory")) else {
            continue;
        };
        if url
            .host_str()
            .is_none_or(|h| h.parse::<std::net::IpAddr>().is_ok())
        {
            continue;
        }
        if seen.insert(url.to_string()) {
            hints.push(url.to_string());
        }
        if hints.len() == 8 {
            break;
        }
    }
    let next = if next >= rows.len() {
        0
    } else {
        i32::try_from(next).unwrap_or_default()
    };
    Ok((hints, next))
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

pub async fn fetch_page(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let source = db::source(&state.pool, lease.source_id)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?
        .ok_or(FederationError::DiscoveryStale)?;
    // Also retire jobs queued by older builds. A typed Lemmy/PeerTube source
    // does not imply support for Mastodon's peer endpoint.
    if !source
        .capabilities
        .iter()
        .any(|capability| capability == "directory" || capability == "peer_exploration")
    {
        db::finish_job(&state.pool, lease)
            .await
            .map_err(|e| FederationError::Admission(e.to_string()))?;
        return Ok(());
    }
    let response = transport::fetch(state, lease, &lease.target_uri, DiscoveryFormat::Json).await?;
    let (hints, next) = parse_page(&response.document, source.peer_offset)?;
    let mut allowed = Vec::new();
    for hint in hints {
        if crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &hint)
            .await
            .map_err(|e| FederationError::Admission(e.to_string()))?
        {
            allowed.push(hint);
        }
    }
    if !(Budget { state, lease }).allowed().await? {
        return Err(FederationError::DiscoveryStale);
    }
    db::finish_peer_page(&state.pool, lease, &allowed, next)
        .await
        .map_err(|e| FederationError::Admission(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn untrusted_addresses_and_bounded_duplicate_windows() {
        let (hints, next) = parse_page(
            &json!([
                "one.example",
                "ONE.example",
                "http://evil.example",
                "evil.example/path",
                "user@evil.example",
                "127.0.0.1",
                "good.onion",
                false
            ]),
            0,
        )
        .unwrap();
        assert_eq!(
            hints,
            vec![
                "https://one.example/api/v1/directory",
                "http://good.onion/api/v1/directory"
            ]
        );
        assert_eq!(next, 0);
        let storm = Value::Array(vec![json!("one.example"); 1000]);
        assert_eq!(parse_page(&storm, 0).unwrap().1, 256);
        assert_eq!(parse_page(&storm, 768).unwrap().1, 0);
        assert!(parse_page(&json!({"peers":[]}), 0).is_err());
        let large = Value::Array(
            (0..40_000)
                .map(|i| json!(format!("peer{i}.example")))
                .collect(),
        );
        let (first, next) = parse_page(&large, 0).unwrap();
        let (second, _) = parse_page(&large, next).unwrap();
        assert_eq!(first.len(), 8);
        assert!(first.iter().all(|uri| !second.contains(uri)));
        assert!(
            first.iter().any(|uri| uri.contains("peer13333.")),
            "window spreads across the list"
        );
    }
}
