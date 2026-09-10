//! `/api/v1/admin/*_blocks` and `domain_allows` — Mastodon's instance-policy
//! admin API. These handlers provide the record surface and resource-
//! specific admin OAuth scopes; federation/access enforcement is built on top
//! of these records separately.

use std::net::IpAddr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use plamenu_db::instance_policy::{self, DomainBlockUpdate, IpBlockUpdate, Page};
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use super::params::parse_body;
use crate::auth::AdminUser;
use crate::entities::{
    admin_canonical_email_block_json, admin_domain_allow_json, admin_domain_block_json,
    admin_email_domain_block_json, admin_ip_block_json, existing_domain_block_error_json,
};
use crate::error::ApiError;
use crate::instance_policy::{canonical_email, normalize_domain, sha256_hex};
use crate::{AppState, admin_log};

const POLICY_LIMIT: i64 = 100;
const DOMAIN_POLICY_MAX_LIMIT: i64 = 500;

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    limit: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DomainInput {
    domain: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DomainBlockInput {
    domain: Option<String>,
    severity: Option<String>,
    reject_media: Option<bool>,
    reject_reports: Option<bool>,
    private_comment: Option<String>,
    public_comment: Option<String>,
    obfuscate: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct EmailDomainBlockInput {
    domain: Option<String>,
    allow_with_approval: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IpBlockInput {
    ip: Option<String>,
    severity: Option<String>,
    comment: Option<String>,
    expires_in: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CanonicalEmailBlockInput {
    canonical_email_hash: Option<String>,
    email: Option<String>,
}

fn page(query: &PageQuery, default_limit: i64, max_limit: i64) -> Page {
    Page {
        max_id: query.max_id,
        since_id: query.since_id,
        min_id: query.min_id,
        limit: query.limit.unwrap_or(default_limit).clamp(1, max_limit),
    }
}

fn link_header(
    state: &AppState,
    path: &str,
    query: &PageQuery,
    limit: i64,
    ids: &[i64],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if ids.is_empty() {
        return headers;
    }
    let ascending = query.min_id.is_some();
    let first = ids.first().copied().unwrap_or_default();
    let last = ids.last().copied().unwrap_or_default();
    let (newest, oldest) = if ascending {
        (first, last)
    } else {
        (last, first)
    };
    let mut links = Vec::new();
    if ids.len() == usize::try_from(limit).unwrap_or(usize::MAX) {
        links.push(format!(
            "<https://{}{path}?limit={limit}&max_id={oldest}>; rel=\"next\"",
            state.config.domain
        ));
    }
    links.push(format!(
        "<https://{}{path}?limit={limit}&min_id={newest}>; rel=\"prev\"",
        state.config.domain
    ));
    if let Ok(value) = links.join(", ").parse() {
        headers.insert(header::LINK, value);
    }
    headers
}

fn domain_severity(value: Option<&str>) -> Result<String, ApiError> {
    let severity = value.unwrap_or("silence").trim();
    match severity {
        "silence" | "suspend" | "noop" => Ok(severity.to_owned()),
        _ => Err(validation("Severity is invalid")),
    }
}

fn ip_severity(value: Option<&str>) -> Result<String, ApiError> {
    let severity = value.unwrap_or("sign_up_block").trim();
    match severity {
        "sign_up_requires_approval" | "sign_up_block" | "no_access" => Ok(severity.to_owned()),
        _ => Err(validation("Severity is invalid")),
    }
}

fn normalize_ip(raw: Option<&str>) -> Result<String, ApiError> {
    let value = raw.unwrap_or_default().trim();
    if value.is_empty() {
        return Err(validation("Ip is invalid"));
    }
    let (addr, prefix) = if let Some((addr, prefix)) = value.split_once('/') {
        let parsed: IpAddr = addr.parse().map_err(|_| validation("Ip is invalid"))?;
        let prefix: u8 = prefix.parse().map_err(|_| validation("Ip is invalid"))?;
        let max = if parsed.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(validation("Ip is invalid"));
        }
        (parsed, prefix)
    } else {
        let parsed: IpAddr = value.parse().map_err(|_| validation("Ip is invalid"))?;
        let prefix = if parsed.is_ipv4() { 32 } else { 128 };
        (parsed, prefix)
    };
    Ok(format!("{addr}/{prefix}"))
}

fn expires_at(expires_in: Option<&str>) -> Result<Option<OffsetDateTime>, ApiError> {
    let Some(raw) = expires_in.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let seconds: i64 = raw
        .parse()
        .map_err(|_| validation("Expires in is invalid"))?;
    if seconds <= 0 {
        return Ok(None);
    }
    Ok(Some(OffsetDateTime::now_utc() + Duration::seconds(seconds)))
}

fn canonical_hash(input: &CanonicalEmailBlockInput) -> Result<String, ApiError> {
    if let Some(hash) = input
        .canonical_email_hash
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(hash.to_owned());
    }
    let email = input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| validation("Email is invalid"))?;
    let canonical = canonical_email(email)?;
    Ok(sha256_hex(&canonical))
}

fn validation(message: &str) -> ApiError {
    ApiError::Unprocessable(format!("Validation failed: {message}"))
}

fn empty() -> Json<Value> {
    Json(json!({}))
}

fn parse_or_query<T>(headers: &HeaderMap, body: &[u8], query: T) -> Result<T, ApiError>
where
    T: serde::de::DeserializeOwned,
{
    if body.is_empty() {
        Ok(query)
    } else {
        parse_body(headers, body)
    }
}

/// `GET /api/v1/admin/domain_blocks`.
pub async fn domain_blocks_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, false, "domain_blocks")?;
    let page = page(&query, POLICY_LIMIT, DOMAIN_POLICY_MAX_LIMIT);
    let rows = instance_policy::list_domain_blocks(&state.pool, &page).await?;
    let body = rows
        .iter()
        .map(admin_domain_block_json)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    Ok((
        link_header(
            &state,
            "/api/v1/admin/domain_blocks",
            &query,
            page.limit,
            &ids,
        ),
        Json(Value::Array(body)),
    ))
}

/// `GET /api/v1/admin/domain_blocks/{id}`.
pub async fn domain_blocks_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, false, "domain_blocks")?;
    let block = instance_policy::find_domain_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(admin_domain_block_json(&block)?))
}

/// `POST /api/v1/admin/domain_blocks`.
pub async fn domain_blocks_create(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<DomainBlockInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(axum::http::StatusCode, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, true, "domain_blocks")?;
    let input = parse_or_query(&headers, &body, query)?;
    let domain = normalize_domain(input.domain.as_deref())?;
    if let Some(existing) =
        instance_policy::find_domain_block_by_domain(&state.pool, &domain).await?
    {
        return Ok((
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(existing_domain_block_error_json(&existing)?),
        ));
    }
    let severity = domain_severity(input.severity.as_deref())?;
    let block = instance_policy::create_domain_block(
        &state.pool,
        instance_policy::NewDomainBlock {
            domain: &domain,
            severity: &severity,
            reject_media: input.reject_media.unwrap_or(false),
            reject_reports: input.reject_reports.unwrap_or(false),
            private_comment: input.private_comment.as_deref(),
            public_comment: input.public_comment.as_deref(),
            obfuscate: input.obfuscate.unwrap_or(false),
        },
    )
    .await?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::domain_block(block.id, &block.domain),
    )
    .await?;
    crate::media_worker::spawn_domain_purge(&state, &block);
    Ok((
        axum::http::StatusCode::OK,
        Json(admin_domain_block_json(&block)?),
    ))
}

/// `PATCH /api/v1/admin/domain_blocks/{id}`.
pub async fn domain_blocks_update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, true, "domain_blocks")?;
    if instance_policy::find_domain_block(&state.pool, id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    let input: DomainBlockInput = parse_body(&headers, &body)?;
    let severity = match input.severity.as_deref() {
        Some(_) => Some(domain_severity(input.severity.as_deref())?),
        None => None,
    };
    let block = instance_policy::update_domain_block(
        &state.pool,
        id,
        DomainBlockUpdate {
            severity: severity.as_deref(),
            reject_media: input.reject_media,
            reject_reports: input.reject_reports,
            private_comment: input.private_comment.as_deref(),
            public_comment: input.public_comment.as_deref(),
            obfuscate: input.obfuscate,
        },
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    log_policy(
        &state,
        &admin,
        "update",
        &admin_log::Target::domain_block(block.id, &block.domain),
    )
    .await?;
    crate::media_worker::spawn_domain_purge(&state, &block);
    Ok(Json(admin_domain_block_json(&block)?))
}

/// `DELETE /api/v1/admin/domain_blocks/{id}`.
pub async fn domain_blocks_destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, true, "domain_blocks")?;
    let block = instance_policy::find_domain_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !instance_policy::delete_domain_block(&state.pool, id).await? {
        return Err(ApiError::NotFound);
    }
    log_policy(
        &state,
        &admin,
        "destroy",
        &admin_log::Target::domain_block(block.id, &block.domain),
    )
    .await?;
    Ok(empty())
}

/// `GET /api/v1/admin/domain_allows`.
pub async fn domain_allows_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, false, "domain_allows")?;
    let page = page(&query, POLICY_LIMIT, DOMAIN_POLICY_MAX_LIMIT);
    let rows = instance_policy::list_domain_allows(&state.pool, &page).await?;
    let body = rows
        .iter()
        .map(admin_domain_allow_json)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    Ok((
        link_header(
            &state,
            "/api/v1/admin/domain_allows",
            &query,
            page.limit,
            &ids,
        ),
        Json(Value::Array(body)),
    ))
}

pub async fn domain_allows_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, false, "domain_allows")?;
    let allow = instance_policy::find_domain_allow(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(admin_domain_allow_json(&allow)?))
}

pub async fn domain_allows_create(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<DomainInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, true, "domain_allows")?;
    let input = parse_or_query(&headers, &body, query)?;
    let domain = normalize_domain(input.domain.as_deref())?;
    let allow = instance_policy::create_domain_allow(&state.pool, &domain).await?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::domain_allow(allow.id, &allow.domain),
    )
    .await?;
    Ok(Json(admin_domain_allow_json(&allow)?))
}

pub async fn domain_allows_destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_FEDERATION, true, "domain_allows")?;
    let allow = instance_policy::find_domain_allow(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !instance_policy::delete_domain_allow(&state.pool, id).await? {
        return Err(ApiError::NotFound);
    }
    log_policy(
        &state,
        &admin,
        "destroy",
        &admin_log::Target::domain_allow(allow.id, &allow.domain),
    )
    .await?;
    Ok(empty())
}

pub async fn email_domain_blocks_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "email_domain_blocks")?;
    let page = page(&query, POLICY_LIMIT, POLICY_LIMIT);
    let rows = instance_policy::list_email_domain_blocks(&state.pool, &page).await?;
    let body = rows
        .iter()
        .map(admin_email_domain_block_json)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    Ok((
        link_header(
            &state,
            "/api/v1/admin/email_domain_blocks",
            &query,
            page.limit,
            &ids,
        ),
        Json(Value::Array(body)),
    ))
}

pub async fn email_domain_blocks_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "email_domain_blocks")?;
    let block = instance_policy::find_email_domain_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(admin_email_domain_block_json(&block)?))
}

pub async fn email_domain_blocks_create(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<EmailDomainBlockInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "email_domain_blocks")?;
    let input = parse_or_query(&headers, &body, query)?;
    let domain = normalize_domain(input.domain.as_deref())?;
    if instance_policy::find_email_domain_block_by_domain(&state.pool, &domain)
        .await?
        .is_some()
    {
        return Err(validation("Domain has already been taken"));
    }
    let block = instance_policy::create_email_domain_block(
        &state.pool,
        &domain,
        input.allow_with_approval.unwrap_or(false),
    )
    .await?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::email_domain_block(block.id, &block.domain),
    )
    .await?;
    Ok(Json(admin_email_domain_block_json(&block)?))
}

pub async fn email_domain_blocks_destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "email_domain_blocks")?;
    let block = instance_policy::find_email_domain_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !instance_policy::delete_email_domain_block(&state.pool, id).await? {
        return Err(ApiError::NotFound);
    }
    log_policy(
        &state,
        &admin,
        "destroy",
        &admin_log::Target::email_domain_block(block.id, &block.domain),
    )
    .await?;
    Ok(empty())
}

pub async fn ip_blocks_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "ip_blocks")?;
    let page = page(&query, POLICY_LIMIT, POLICY_LIMIT);
    let rows = instance_policy::list_ip_blocks(&state.pool, &page).await?;
    let body = rows
        .iter()
        .map(admin_ip_block_json)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    Ok((
        link_header(&state, "/api/v1/admin/ip_blocks", &query, page.limit, &ids),
        Json(Value::Array(body)),
    ))
}

pub async fn ip_blocks_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "ip_blocks")?;
    let block = instance_policy::find_ip_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(admin_ip_block_json(&block)?))
}

pub async fn ip_blocks_create(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<IpBlockInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "ip_blocks")?;
    let input = parse_or_query(&headers, &body, query)?;
    let ip = normalize_ip(input.ip.as_deref())?;
    if instance_policy::find_ip_block_by_ip(&state.pool, &ip)
        .await?
        .is_some()
    {
        return Err(validation("Ip has already been taken"));
    }
    let severity = ip_severity(input.severity.as_deref())?;
    let block = instance_policy::create_ip_block(
        &state.pool,
        &ip,
        &severity,
        input.comment.as_deref().unwrap_or_default(),
        expires_at(input.expires_in.as_deref())?,
    )
    .await?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::ip_block(block.id, &block.ip),
    )
    .await?;
    Ok(Json(admin_ip_block_json(&block)?))
}

pub async fn ip_blocks_update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "ip_blocks")?;
    let current = instance_policy::find_ip_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let input: IpBlockInput = parse_body(&headers, &body)?;
    let ip = match input.ip.as_deref() {
        Some(_) => Some(normalize_ip(input.ip.as_deref())?),
        None => None,
    };
    if let Some(ip) = ip.as_deref()
        && ip != current.ip
        && instance_policy::find_ip_block_by_ip(&state.pool, ip)
            .await?
            .is_some()
    {
        return Err(validation("Ip has already been taken"));
    }
    let severity = match input.severity.as_deref() {
        Some(_) => Some(ip_severity(input.severity.as_deref())?),
        None => None,
    };
    let block = instance_policy::update_ip_block(
        &state.pool,
        id,
        IpBlockUpdate {
            ip: ip.as_deref(),
            severity: severity.as_deref(),
            comment: input.comment.as_deref(),
            update_expires_at: input.expires_in.is_some(),
            expires_at: expires_at(input.expires_in.as_deref())?,
        },
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    log_policy(
        &state,
        &admin,
        "update",
        &admin_log::Target::ip_block(block.id, &block.ip),
    )
    .await?;
    Ok(Json(admin_ip_block_json(&block)?))
}

pub async fn ip_blocks_destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "ip_blocks")?;
    let block = instance_policy::find_ip_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !instance_policy::delete_ip_block(&state.pool, id).await? {
        return Err(ApiError::NotFound);
    }
    log_policy(
        &state,
        &admin,
        "destroy",
        &admin_log::Target::ip_block(block.id, &block.ip),
    )
    .await?;
    Ok(empty())
}

pub async fn canonical_email_blocks_index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<PageQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "canonical_email_blocks")?;
    let page = page(&query, POLICY_LIMIT, POLICY_LIMIT);
    let rows = instance_policy::list_canonical_email_blocks(&state.pool, &page).await?;
    let body = rows
        .iter()
        .map(admin_canonical_email_block_json)
        .collect::<Vec<_>>();
    let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
    Ok((
        link_header(
            &state,
            "/api/v1/admin/canonical_email_blocks",
            &query,
            page.limit,
            &ids,
        ),
        Json(Value::Array(body)),
    ))
}

pub async fn canonical_email_blocks_show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "canonical_email_blocks")?;
    let block = instance_policy::find_canonical_email_block(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(admin_canonical_email_block_json(&block)))
}

pub async fn canonical_email_blocks_test(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<CanonicalEmailBlockInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, false, "canonical_email_blocks")?;
    let input = parse_or_query(&headers, &body, query)?;
    let hash = canonical_hash(&input)?;
    let rows = instance_policy::matching_canonical_email_blocks(&state.pool, &hash).await?;
    Ok(Json(Value::Array(
        rows.iter().map(admin_canonical_email_block_json).collect(),
    )))
}

pub async fn canonical_email_blocks_create(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<CanonicalEmailBlockInput>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "canonical_email_blocks")?;
    let input = parse_or_query(&headers, &body, query)?;
    let hash = canonical_hash(&input)?;
    if instance_policy::find_canonical_email_block_by_hash(&state.pool, &hash)
        .await?
        .is_some()
    {
        return Err(validation("Canonical email hash has already been taken"));
    }
    let block = instance_policy::create_canonical_email_block(&state.pool, &hash, None).await?;
    log_policy(
        &state,
        &admin,
        "create",
        &admin_log::Target::canonical_email_block(block.id),
    )
    .await?;
    Ok(Json(admin_canonical_email_block_json(&block)))
}

pub async fn canonical_email_blocks_destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_BLOCKS, true, "canonical_email_blocks")?;
    if !instance_policy::delete_canonical_email_block(&state.pool, id).await? {
        return Err(ApiError::NotFound);
    }
    log_policy(
        &state,
        &admin,
        "destroy",
        &admin_log::Target::canonical_email_block(id),
    )
    .await?;
    Ok(empty())
}

/// Appends a policy verb to the audit log.
async fn log_policy(
    state: &AppState,
    admin: &AdminUser,
    verb: &str,
    target: &admin_log::Target,
) -> Result<(), ApiError> {
    admin_log::record(&state.pool, admin.current.account.id, verb, target).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{canonical_email, normalize_domain, normalize_ip};

    #[test]
    fn normalizes_admin_policy_inputs() {
        assert_eq!(
            normalize_domain(Some("Example.COM")).unwrap(),
            "example.com"
        );
        assert!(normalize_domain(Some("https://example.com")).is_err());
        assert_eq!(normalize_ip(Some("192.0.2.3")).unwrap(), "192.0.2.3/32");
        assert_eq!(normalize_ip(Some("192.0.2.0/24")).unwrap(), "192.0.2.0/24");
        assert_eq!(
            canonical_email("First.Last+tag@Example.COM").unwrap(),
            "firstlast@example.com"
        );
    }
}
