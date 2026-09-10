//! `/api/v1/domain_blocks*` — user-level remote-domain blocking.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::account_domain_block;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::actions;
use crate::auth::CurrentUser;
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct ListQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct DomainParams {
    domain: Option<String>,
}

pub(crate) fn normalize_domain(raw: Option<&str>) -> Result<String, ApiError> {
    let trimmed = raw.unwrap_or_default().trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('@')
        || trimmed.contains('?')
        || trimmed.contains('#')
    {
        return Err(ApiError::Unprocessable(
            "Validation failed: Domain is invalid".into(),
        ));
    }
    let parsed = Url::parse(&format!("https://{trimmed}/"))
        .map_err(|_| ApiError::Unprocessable("Validation failed: Domain is invalid".into()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| ApiError::Unprocessable("Validation failed: Domain is invalid".into()))?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ApiError::Unprocessable(
            "Validation failed: Domain is invalid".into(),
        ));
    }
    let normalized = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    if normalized.is_empty() {
        return Err(ApiError::Unprocessable(
            "Validation failed: Domain is invalid".into(),
        ));
    }
    Ok(normalized)
}

fn domain_link_header(
    domain: &str,
    limit: i64,
    page: &[account_domain_block::DomainBlockListEntry],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut links = Vec::new();
    if page.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = page.last()
    {
        links.push(format!(
            "<https://{domain}/api/v1/domain_blocks?limit={limit}&max_id={}>; rel=\"next\"",
            last.row_id
        ));
    }
    if let Some(first) = page.first() {
        links.push(format!(
            "<https://{domain}/api/v1/domain_blocks?limit={limit}&since_id={}>; rel=\"prev\"",
            first.row_id
        ));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    headers
}

/// `GET /api/v1/domain_blocks` — blocked domains as strings, newest first.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if current.require_scope("read:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let page = account_domain_block::list(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        limit,
    )
    .await?;
    let body = page
        .iter()
        .map(|entry| Value::String(entry.domain.clone()))
        .collect();
    let headers = domain_link_header(&state.config.domain, limit, &page);
    Ok((headers, Json(Value::Array(body))))
}

/// `GET /api/v1/domain_blocks/preview` — relationship counts that blocking
/// this domain would sever.
pub async fn preview(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<DomainParams>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let domain = normalize_domain(query.domain.as_deref())?;
    let preview = account_domain_block::preview(&state.pool, current.account.id, &domain).await?;
    Ok(Json(json!({
        "following_count": preview.following_count,
        "followers_count": preview.followers_count,
    })))
}

/// `POST /api/v1/domain_blocks` — block a remote domain, cleaning up follows.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<DomainParams>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let params: DomainParams = if body.is_empty() {
        query
    } else {
        super::params::parse_body(&headers, &body)?
    };
    let domain = normalize_domain(params.domain.as_deref())?;
    actions::block_domain(&state, &current.account, &domain).await?;
    Ok(Json(json!({})))
}

/// `DELETE /api/v1/domain_blocks` — unblock a domain. Missing blocks are OK.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<DomainParams>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:blocks").is_err() {
        current.require_scope("follow")?;
    }
    let params: DomainParams = if body.is_empty() {
        query
    } else {
        super::params::parse_body(&headers, &body)?
    };
    let domain = normalize_domain(params.domain.as_deref())?;
    actions::unblock_domain(&state, &current.account, &domain).await?;
    Ok(Json(json!({})))
}

#[cfg(test)]
mod tests {
    use super::normalize_domain;

    #[test]
    fn normalizes_domains_like_api_params() {
        assert_eq!(
            normalize_domain(Some(" Example.COM/ ")).unwrap(),
            "example.com"
        );
        assert_eq!(
            normalize_domain(Some("plamenu.local:8420")).unwrap(),
            "plamenu.local:8420"
        );
        assert!(normalize_domain(Some("example com")).is_err());
        assert!(normalize_domain(Some("https://example.com")).is_err());
        assert!(normalize_domain(Some("example.com?x")).is_err());
        assert!(normalize_domain(Some("example.com#x")).is_err());
        assert!(normalize_domain(Some("")).is_err());
    }
}
