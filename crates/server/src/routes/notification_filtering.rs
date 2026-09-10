//! Notification filtering surface: the per-account policy
//! (`/api/v{1,2}/notifications/policy`) and the filtered-notification requests
//! (`/api/v1/notifications/requests`). See [`plamenu_db::notification_policy`]
//! for the filtering decision and [`plamenu_db::notification_request`] for the
//! per-sender rollup.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::notification_policy::{self, Disposition, Policy};
use plamenu_db::notification_request::{self, NotificationRequest};
use plamenu_db::{account, status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::{parse_body, repeated_form_field};
use crate::auth::CurrentUser;
use crate::entities::{account_json, render_statuses, rfc3339};
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 40;
const MAX_LIMIT: i64 = 80;
/// Mastodon caps the policy summary counts at `MAX_MEANINGFUL_COUNT`.
const MAX_MEANINGFUL_COUNT: i64 = 100;

// ---------------------------------------------------------------------------
// Policy — /api/v1/notifications/policy and /api/v2/notifications/policy

/// The `summary` object shared by both policy serializers.
async fn summary_json(state: &AppState, account_id: i64) -> Result<Value, ApiError> {
    let (requests, notifications) = notification_policy::summary(&state.pool, account_id).await?;
    Ok(json!({
        "pending_requests_count": requests.min(MAX_MEANINGFUL_COUNT),
        "pending_notifications_count": notifications.min(MAX_MEANINGFUL_COUNT),
    }))
}

/// Mastodon's `REST::V1::NotificationPolicySerializer`: booleans that read
/// "is this category at all filtered" (`drop` shows as `true` too).
fn policy_v1_json(policy: Policy, summary: Value) -> Value {
    let filtered = |d: Disposition| d != Disposition::Accept;
    let mut entity = json!({
        "filter_not_following": filtered(policy.for_not_following),
        "filter_not_followers": filtered(policy.for_not_followers),
        "filter_new_accounts": filtered(policy.for_new_accounts),
        "filter_private_mentions": filtered(policy.for_private_mentions),
        "filter_bots": filtered(policy.for_bots),
    });
    entity["summary"] = summary;
    entity
}

/// Mastodon's `REST::NotificationPolicySerializer` (v2): the per-category
/// `accept`/`filter`/`drop` enum.
fn policy_v2_json(policy: Policy, summary: Value) -> Value {
    let mut entity = json!({
        "for_not_following": policy.for_not_following.as_str(),
        "for_not_followers": policy.for_not_followers.as_str(),
        "for_new_accounts": policy.for_new_accounts.as_str(),
        "for_private_mentions": policy.for_private_mentions.as_str(),
        "for_limited_accounts": policy.for_limited_accounts.as_str(),
        "for_bots": policy.for_bots.as_str(),
    });
    entity["summary"] = summary;
    entity
}

/// `GET /api/v1/notifications/policy`.
pub async fn policy_v1(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let policy = notification_policy::get_or_default(&state.pool, current.account.id).await?;
    let summary = summary_json(&state, current.account.id).await?;
    Ok(Json(policy_v1_json(policy, summary)))
}

/// `GET /api/v2/notifications/policy`.
pub async fn policy_v2(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let policy = notification_policy::get_or_default(&state.pool, current.account.id).await?;
    let summary = summary_json(&state, current.account.id).await?;
    Ok(Json(policy_v2_json(policy, summary)))
}

/// v1's booleans only toggle between `accept` and `filter` (Mastodon's
/// `filter_*=` compat writers); `drop` is unreachable from v1.
#[derive(Deserialize, Default)]
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the Mastodon API params"
)]
struct PolicyV1Body {
    filter_not_following: Option<bool>,
    filter_not_followers: Option<bool>,
    filter_new_accounts: Option<bool>,
    filter_private_mentions: Option<bool>,
    filter_bots: Option<bool>,
}

#[derive(Deserialize, Default)]
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the Mastodon API params"
)]
struct PolicyV2Body {
    for_not_following: Option<String>,
    for_not_followers: Option<String>,
    for_new_accounts: Option<String>,
    for_private_mentions: Option<String>,
    for_limited_accounts: Option<String>,
    for_bots: Option<String>,
}

/// `PATCH /api/v1/notifications/policy` — a partial update, like Mastodon's
/// permitted-params `update!`.
pub async fn update_policy_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let changes: PolicyV1Body = parse_body(&headers, &body)?;
    let mut policy = notification_policy::get_or_default(&state.pool, current.account.id).await?;
    let toggle = |b: bool| {
        if b {
            Disposition::Filter
        } else {
            Disposition::Accept
        }
    };
    if let Some(b) = changes.filter_not_following {
        policy.for_not_following = toggle(b);
    }
    if let Some(b) = changes.filter_not_followers {
        policy.for_not_followers = toggle(b);
    }
    if let Some(b) = changes.filter_new_accounts {
        policy.for_new_accounts = toggle(b);
    }
    if let Some(b) = changes.filter_private_mentions {
        policy.for_private_mentions = toggle(b);
    }
    if let Some(b) = changes.filter_bots {
        policy.for_bots = toggle(b);
    }
    let policy = notification_policy::upsert(&state.pool, current.account.id, policy).await?;
    let summary = summary_json(&state, current.account.id).await?;
    Ok(Json(policy_v1_json(policy, summary)))
}

/// `PATCH /api/v2/notifications/policy` — a partial update.
pub async fn update_policy_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let changes: PolicyV2Body = parse_body(&headers, &body)?;
    let mut policy = notification_policy::get_or_default(&state.pool, current.account.id).await?;
    for (value, slot) in [
        (changes.for_not_following, &mut policy.for_not_following),
        (changes.for_not_followers, &mut policy.for_not_followers),
        (changes.for_new_accounts, &mut policy.for_new_accounts),
        (
            changes.for_private_mentions,
            &mut policy.for_private_mentions,
        ),
        (
            changes.for_limited_accounts,
            &mut policy.for_limited_accounts,
        ),
        (changes.for_bots, &mut policy.for_bots),
    ] {
        if let Some(value) = value {
            *slot = Disposition::parse(&value);
        }
    }
    let policy = notification_policy::upsert(&state.pool, current.account.id, policy).await?;
    let summary = summary_json(&state, current.account.id).await?;
    Ok(Json(policy_v2_json(policy, summary)))
}

// ---------------------------------------------------------------------------
// Requests — /api/v1/notifications/requests

#[derive(Deserialize)]
pub struct RequestParams {
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: Option<i64>,
}

/// The Mastodon `NotificationRequest` entity.
async fn request_json(
    state: &AppState,
    viewer: i64,
    request: &NotificationRequest,
) -> Result<Value, ApiError> {
    let from = account::find_by_id(&state.pool, request.from_account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let account_entity =
        account_json(&state.pool, &state.config.domain, &from, Some(viewer)).await?;
    let last_status = match request.last_status_id {
        Some(status_id) => match status::find_by_id(&state.pool, status_id).await? {
            Some(item) => render_statuses(
                &state.pool,
                &state.config.domain,
                std::slice::from_ref(&item),
                Some(viewer),
            )
            .await?
            .pop()
            .unwrap_or(Value::Null),
            None => Value::Null,
        },
        None => Value::Null,
    };
    Ok(json!({
        "id": request.id.to_string(),
        "created_at": rfc3339(request.created_at)?,
        "updated_at": rfc3339(request.updated_at)?,
        "notifications_count": request.notifications_count.to_string(),
        "account": account_entity,
        "last_status": last_status,
    }))
}

fn requests_link_header(domain: &str, limit: i64, page: &[NotificationRequest]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut parts = Vec::new();
    let path = "/api/v1/notifications/requests";
    if let Some(last) = page.last() {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&max_id={}>; rel=\"next\"",
            last.id
        ));
    }
    if let Some(first) = page.first() {
        parts.push(format!(
            "<https://{domain}{path}?limit={limit}&min_id={}>; rel=\"prev\"",
            first.id
        ));
    }
    if !parts.is_empty()
        && let Ok(value) = parts.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    headers
}

/// `GET /api/v1/notifications/requests`.
pub async fn requests_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<RequestParams>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:notifications")?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let requests = notification_request::list(
        &state.pool,
        current.account.id,
        params.max_id,
        params.since_id,
        params.min_id,
        limit,
    )
    .await?;
    let mut entities = Vec::with_capacity(requests.len());
    for request in &requests {
        entities.push(request_json(&state, current.account.id, request).await?);
    }
    let headers = requests_link_header(&state.config.domain, limit, &requests);
    Ok((headers, Json(Value::Array(entities))))
}

/// `GET /api/v1/notifications/requests/merged` — Plamenu unfilters
/// synchronously on accept, so there is never a pending merge backlog.
pub async fn requests_merged(current: CurrentUser) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    Ok(Json(json!({ "merged": true })))
}

/// `GET /api/v1/notifications/requests/{id}`.
pub async fn request_show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:notifications")?;
    let request = notification_request::find(&state.pool, current.account.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        request_json(&state, current.account.id, &request).await?,
    ))
}

/// `POST /api/v1/notifications/requests/{id}/accept`.
pub async fn accept_request(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let request = notification_request::find(&state.pool, current.account.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    notification_request::accept(&state.pool, current.account.id, request.from_account_id).await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/notifications/requests/{id}/dismiss`.
pub async fn dismiss_request(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let request = notification_request::find(&state.pool, current.account.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    notification_request::dismiss(&state.pool, current.account.id, request.from_account_id).await?;
    Ok(Json(json!({})))
}

/// Distinct notification-request ids accepted per bulk accept/dismiss. The
/// downstream work is already bounded by the caller's real pending requests,
/// but capping + deduplicating the input keeps a repeated or oversized array
/// from the ~2 MiB body cheap to parse and dispatch.
const MAX_NOTIFICATION_REQUEST_IDS: usize = 100;

/// The `id[]` array for the bulk endpoints (JSON `{"id":[…]}` or form `id[]=…`).
fn bulk_ids(headers: &HeaderMap, body: &[u8]) -> Result<Vec<i64>, ApiError> {
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    let ids = if is_json {
        #[derive(Deserialize, Default)]
        struct Body {
            #[serde(default)]
            id: Vec<i64>,
        }
        let parsed: Body = parse_body(headers, body)?;
        parsed.id
    } else {
        repeated_form_field(headers, body, "id")
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    super::params::bounded_unique_ids(ids, MAX_NOTIFICATION_REQUEST_IDS)
}

/// `POST /api/v1/notifications/requests/accept` (bulk).
pub async fn accept_requests(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let ids = bulk_ids(&headers, &body)?;
    let requests = notification_request::get_many(&state.pool, current.account.id, &ids).await?;
    for request in requests {
        notification_request::accept(&state.pool, current.account.id, request.from_account_id)
            .await?;
    }
    Ok(Json(json!({})))
}

/// `POST /api/v1/notifications/requests/dismiss` (bulk).
pub async fn dismiss_requests(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:notifications")?;
    let ids = bulk_ids(&headers, &body)?;
    let requests = notification_request::get_many(&state.pool, current.account.id, &ids).await?;
    for request in requests {
        notification_request::dismiss(&state.pool, current.account.id, request.from_account_id)
            .await?;
    }
    Ok(Json(json!({})))
}
