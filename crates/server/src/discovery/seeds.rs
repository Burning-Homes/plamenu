//! Replaceable, bounded source manifests. Imported facts are never evidence.
use plamenu_db::{DbError, PgPool, discovery_store as db};
use serde::Deserialize;

pub const BUNDLED: &str = include_str!("seeds.json");
pub const MAX_BYTES: usize = 65536;

#[derive(Deserialize)]
struct Manifest {
    version: u8,
    sources: Vec<Hint>,
}
#[derive(Deserialize)]
struct Hint {
    origin: String,
    endpoint: Option<String>,
}

pub fn parse(raw: &str) -> Result<Vec<String>, String> {
    if raw.len() > MAX_BYTES {
        return Err("Source list exceeds 64 KiB".into());
    }
    let manifest: Manifest = serde_json::from_str(raw).map_err(|_| "Invalid source list JSON")?;
    if manifest.version != 1 || manifest.sources.len() > 100 {
        return Err("Use source list version 1 with at most 100 sources".into());
    }
    let mut endpoints = std::collections::BTreeSet::new();
    for hint in manifest.sources {
        let origin =
            super::directory::endpoint(&hint.origin).map_err(|_| "Invalid source origin")?;
        let endpoint = match hint.endpoint {
            None => origin.clone(),
            Some(endpoint) => {
                let endpoint =
                    super::videos::endpoint(&endpoint).map_err(|_| "Invalid source endpoint")?;
                if !super::outbox::same_origin(&endpoint, &origin) {
                    return Err("Source endpoint left its origin".into());
                }
                endpoint
            }
        };
        endpoints.insert(endpoint);
    }
    Ok(endpoints.into_iter().collect())
}

pub async fn import(
    pool: &PgPool,
    raw: &str,
    bundled: bool,
    replace: bool,
) -> Result<db::ImportResult, DbError> {
    let endpoints = parse(raw).map_err(DbError::Protocol)?;
    db::import_sources(
        pool,
        &endpoints,
        if bundled { "bundled" } else { "import" },
        replace,
    )
    .await
}

pub async fn export(pool: &PgPool) -> Result<String, DbError> {
    let sources = db::source_diagnostics(pool).await?;
    let sources: Vec<_> = sources
        .into_iter()
        .take(100)
        .map(|source| {
            serde_json::json!({
                "origin": source.origin,
                "endpoint": source.endpoint,
                "observed_capabilities": source.capabilities,
                "directory_checked_at": source.verified_at.map(|t| t.to_string()),
                "peers_checked_at": source.peers_verified_at.map(|t| t.to_string()),
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({"version":1,"sources":sources}))
        .map_err(|e| DbError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imported_facts_are_ignored_and_all_addresses_validated_first() {
        assert_eq!(parse(r#"{"version":1,"sources":[{"origin":"https://one.example","verified":true,"discoverable":true},{"origin":"https://one.example/"}]}"#).unwrap(),vec!["https://one.example/api/v1/directory"]);
        assert!(parse(r#"{"version":1,"sources":[{"origin":"https://one.example"},{"origin":"https://user:password@two.example"}]}"#).is_err());
        assert!(parse(&" ".repeat(MAX_BYTES + 1)).is_err());
        assert_eq!(parse(BUNDLED).unwrap().len(), 9);
        assert_eq!(parse(r#"{"version":1,"sources":[{"origin":"https://groups.example","endpoint":"https://groups.example/api/v3/community/list"},{"origin":"https://films.example","endpoint":"https://films.example/api/v1/videos"}]}"#).unwrap(),vec!["https://films.example/api/v1/videos","https://groups.example/api/v3/community/list"]);
        assert!(parse(r#"{"version":1,"sources":[{"origin":"https://one.example","endpoint":"https://other.example/api/v1/videos"}]}"#).is_err());
    }
}
