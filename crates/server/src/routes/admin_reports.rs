//! `/api/v1/admin/reports` — the admin report moderation surface (Mastodon's
//! admin reports API): list/show plus the
//! `update`/`assign_to_self`/`unassign`/`reopen`/`resolve` verbs. Handlers
//! require `MANAGE_REPORTS` and either broad `admin:read`/`admin:write` or
//! `admin:*:reports`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::report::{self, AdminReportFilter, Report};
use plamenu_db::role::permission;
use plamenu_db::webhook;
use serde::Deserialize;
use serde_json::Value;

use super::params::truthy;
use crate::auth::AdminUser;
use crate::entities::admin_report_json;
use crate::error::ApiError;
use crate::{AppState, admin_log};

/// Mastodon's admin reports `LIMIT` and its doubled hard cap (`limit_param`).
const ADMIN_REPORTS_LIMIT: i64 = 100;
const ADMIN_REPORTS_MAX_LIMIT: i64 = 200;

/// `GET /api/v1/admin/reports` filter + pagination params (the controller's
/// `FILTER_PARAMS` + keyset cursor).
#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    limit: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    resolved: Option<String>,
    unresolved: Option<String>,
    account_id: Option<i64>,
    target_account_id: Option<i64>,
}

/// `GET /api/v1/admin/reports`.
pub async fn index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<IndexQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, false, "reports")?;

    let limit = query
        .limit
        .unwrap_or(ADMIN_REPORTS_LIMIT)
        .clamp(1, ADMIN_REPORTS_MAX_LIMIT);
    let filter = AdminReportFilter {
        resolved: truthy(query.resolved.as_deref()),
        unresolved: truthy(query.unresolved.as_deref()),
        account_id: query.account_id,
        target_account_id: query.target_account_id,
        // The instance-staff console lists every report, group-scoped or not.
        group_account_id: None,
        max_id: query.max_id,
        since_id: query.since_id,
        min_id: query.min_id,
        limit,
    };
    let reports = report::list_for_admin(&state.pool, &filter).await?;

    let mut out = Vec::with_capacity(reports.len());
    for report in &reports {
        out.push(admin_report_json(&state.pool, &state.config.domain, report).await?);
    }
    let headers = link_header(&state, &query, limit, &reports);
    Ok((headers, Json(Value::Array(out))))
}

/// `GET /api/v1/admin/reports/{id}`.
pub async fn show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, false, "reports")?;
    let report = report::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    render(&state, &report).await
}

/// `PUT`/`PATCH /api/v1/admin/reports/{id}` — update the report's
/// `category`/`rule_ids` (Rails `resources :update` answers both verbs).
pub async fn update(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, true, "reports")?;
    // 404 before parsing, like Mastodon's `set_report` before-action.
    if report::find_by_id(&state.pool, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let input = parse_update(&headers, &body)?;
    let report = report::update_category(
        &state.pool,
        id,
        input.category.as_deref(),
        input.rule_ids.as_deref(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    crate::webhooks::report_event(&state, webhook::REPORT_UPDATED, &report).await;
    log_report(&state, &admin, "update", id).await?;
    render(&state, &report).await
}

/// `POST /api/v1/admin/reports/{id}/assign_to_self`.
pub async fn assign_to_self(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, true, "reports")?;
    let report = report::assign(&state.pool, id, Some(admin.current.account.id))
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::webhooks::report_event(&state, webhook::REPORT_UPDATED, &report).await;
    log_report(&state, &admin, "assigned_to_self", id).await?;
    render(&state, &report).await
}

/// `POST /api/v1/admin/reports/{id}/unassign`.
pub async fn unassign(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, true, "reports")?;
    let report = report::assign(&state.pool, id, None)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::webhooks::report_event(&state, webhook::REPORT_UPDATED, &report).await;
    log_report(&state, &admin, "unassigned", id).await?;
    render(&state, &report).await
}

/// `POST /api/v1/admin/reports/{id}/reopen`.
pub async fn reopen(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, true, "reports")?;
    let report = report::unresolve(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::webhooks::report_event(&state, webhook::REPORT_UPDATED, &report).await;
    log_report(&state, &admin, "reopen", id).await?;
    render(&state, &report).await
}

/// `POST /api/v1/admin/reports/{id}/resolve`.
pub async fn resolve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_REPORTS, true, "reports")?;
    let report = report::resolve(&state.pool, id, admin.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::webhooks::report_event(&state, webhook::REPORT_UPDATED, &report).await;
    log_report(&state, &admin, "resolve", id).await?;
    render(&state, &report).await
}

/// Appends the report verb to the audit log.
async fn log_report(
    state: &AppState,
    admin: &AdminUser,
    verb: &str,
    report_id: i64,
) -> Result<(), ApiError> {
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        verb,
        &admin_log::Target::report(report_id),
    )
    .await?;
    Ok(())
}

/// The fields `PUT report_params` permits: `category` and `rule_ids`.
#[derive(Default)]
struct UpdateInput {
    category: Option<String>,
    rule_ids: Option<Vec<i64>>,
}

/// Decodes the update body from JSON (`{category, rule_ids:[…]}`) or a form
/// (`category=…&rule_ids[]=…`). An absent key leaves that field untouched.
fn parse_update(headers: &HeaderMap, body: &[u8]) -> Result<UpdateInput, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
        let category = value
            .get("category")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let rule_ids = value.get("rule_ids").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| match v {
                    Value::String(s) => s.parse().ok(),
                    Value::Number(n) => n.as_i64(),
                    _ => None,
                })
                .collect()
        });
        Ok(UpdateInput { category, rule_ids })
    } else {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
        let mut input = UpdateInput::default();
        let mut rule_ids: Option<Vec<i64>> = None;
        for (key, value) in pairs {
            match key.as_str() {
                "category" => input.category = Some(value),
                "rule_ids" | "rule_ids[]" => {
                    let list = rule_ids.get_or_insert_with(Vec::new);
                    if let Ok(id) = value.parse() {
                        list.push(id);
                    }
                }
                _ => {}
            }
        }
        input.rule_ids = rule_ids;
        Ok(input)
    }
}

/// Re-renders the `Admin::Report` serializer after a read/action.
async fn render(state: &AppState, report: &Report) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        admin_report_json(&state.pool, &state.config.domain, report).await?,
    ))
}

/// Mastodon's id-keyset `Link` header: `next` once a full page comes back,
/// `prev` whenever the page is non-empty (mirrors `admin_accounts`).
fn link_header(state: &AppState, query: &IndexQuery, limit: i64, reports: &[Report]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if reports.is_empty() {
        return headers;
    }
    let domain = &state.config.domain;
    let path = "/api/v1/admin/reports";
    let ascending = query.min_id.is_some();
    let first = reports.first().map_or(0, |r| r.id);
    let last = reports.last().map_or(0, |r| r.id);
    let (newest, oldest) = if ascending {
        (first, last)
    } else {
        (last, first)
    };
    let mut links = Vec::new();
    if reports.len() == usize::try_from(limit).unwrap_or(usize::MAX) {
        links.push(format!(
            "<https://{domain}{path}?limit={limit}&max_id={oldest}>; rel=\"next\""
        ));
    }
    links.push(format!(
        "<https://{domain}{path}?limit={limit}&min_id={newest}>; rel=\"prev\""
    ));
    if let Ok(value) = links.join(", ").parse() {
        headers.insert(axum::http::header::LINK, value);
    }
    headers
}
