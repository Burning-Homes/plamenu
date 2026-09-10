//! `/api/v1|v2/admin/accounts` — the admin account moderation listing and
//! show endpoints (Mastodon's `Api::V1::Admin::AccountsController` and the v2
//! variant) plus the action verbs. Handlers require `MANAGE_USERS` and either
//! broad `admin:read`/`admin:write` or `admin:*:accounts`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::admin_account::{self, AdminAccountFilter, AdminAccountView};
use plamenu_db::role::{self, permission};
use plamenu_db::{account, user};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::truthy;
use crate::auth::AdminUser;
use crate::entities::admin_account_json;
use crate::error::ApiError;
use crate::{AppState, admin_log};

/// Mastodon's admin `LIMIT` and its doubled hard cap (`limit_param`).
const ADMIN_ACCOUNTS_LIMIT: i64 = 100;
const ADMIN_ACCOUNTS_MAX_LIMIT: i64 = 200;

/// Shared keyset cursor + page-size params.
#[derive(Debug, Default, Deserialize)]
pub struct Pagination {
    limit: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
}

/// The v1 filter params (`Api::V1::Admin::AccountsController::FILTER_PARAMS`),
/// the deprecated boolean-flag flavour.
#[derive(Debug, Default, Deserialize)]
pub struct V1Query {
    #[serde(flatten)]
    page: Pagination,
    local: Option<String>,
    remote: Option<String>,
    by_domain: Option<String>,
    active: Option<String>,
    pending: Option<String>,
    disabled: Option<String>,
    sensitized: Option<String>,
    silenced: Option<String>,
    suspended: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    email: Option<String>,
    staff: Option<String>,
}

/// The v2 filter params (`Api::V2::Admin::AccountsController::FILTER_PARAMS`),
/// the current named-value flavour.
#[derive(Debug, Default, Deserialize)]
pub struct V2Query {
    #[serde(flatten)]
    page: Pagination,
    origin: Option<String>,
    status: Option<String>,
    permissions: Option<String>,
    by_domain: Option<String>,
    username: Option<String>,
    display_name: Option<String>,
    email: Option<String>,
    #[serde(default, rename = "role_ids")]
    role_ids: Vec<String>,
}

/// `GET /api/v1/admin/accounts`.
pub async fn index_v1(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<V1Query>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_USERS, false, "accounts")?;

    let origin = match (
        truthy(query.local.as_deref()),
        truthy(query.remote.as_deref()),
    ) {
        (true, false) => Some("local".to_owned()),
        (false, true) => Some("remote".to_owned()),
        _ => None,
    };
    // The first truthy status flag wins, matching Mastodon's filter precedence.
    let status = [
        ("active", &query.active),
        ("pending", &query.pending),
        ("disabled", &query.disabled),
        ("silenced", &query.silenced),
        ("suspended", &query.suspended),
        ("sensitized", &query.sensitized),
    ]
    .into_iter()
    .find(|(_, flag)| truthy(flag.as_deref()))
    .map(|(name, _)| name.to_owned());

    let role_ids = if truthy(query.staff.as_deref()) {
        staff_role_ids(&state).await?
    } else {
        Vec::new()
    };

    let filter = AdminAccountFilter {
        origin,
        status,
        by_domain: query.by_domain,
        username: prefix(query.username),
        display_name: prefix(query.display_name),
        email: prefix(query.email),
        role_ids,
        ..cursor(&query.page)
    };
    respond(&state, "/api/v1/admin/accounts", &query.page, filter).await
}

/// `GET /api/v2/admin/accounts`.
pub async fn index_v2(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(query): Query<V2Query>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    admin.require_resource(permission::MANAGE_USERS, false, "accounts")?;

    // `permissions=staff` is Mastodon's shorthand for the manage-reports roles.
    let mut role_ids: Vec<i64> = query
        .role_ids
        .iter()
        .filter_map(|r| r.parse().ok())
        .collect();
    if query.permissions.as_deref() == Some("staff") {
        role_ids.extend(staff_role_ids(&state).await?);
    }

    let filter = AdminAccountFilter {
        origin: query.origin,
        status: query.status,
        by_domain: query.by_domain,
        username: prefix(query.username),
        display_name: prefix(query.display_name),
        email: prefix(query.email),
        role_ids,
        ..cursor(&query.page)
    };
    respond(&state, "/api/v2/admin/accounts", &query.page, filter).await
}

/// `GET /api/v1/admin/accounts/{id}`.
pub async fn show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, false, "accounts")?;
    let view = admin_account::show(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        admin_account_json(&state.pool, &state.config.domain, &view).await?,
    ))
}

/// Builds a filter carrying just the resolved pagination cursor (the rest of
/// the fields are overwritten by the caller via struct update syntax).
fn cursor(page: &Pagination) -> AdminAccountFilter {
    AdminAccountFilter {
        max_id: page.max_id,
        since_id: page.since_id,
        min_id: page.min_id,
        limit: page
            .limit
            .unwrap_or(ADMIN_ACCOUNTS_LIMIT)
            .clamp(1, ADMIN_ACCOUNTS_MAX_LIMIT),
        ..AdminAccountFilter::default()
    }
}

/// Mastodon prefix-matches `username`/`display_name`/`email` (`value%`, ILIKE).
fn prefix(value: Option<String>) -> Option<String> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| format!("{}%", v.replace('%', "\\%").replace('_', "\\_")))
}

/// The roles that can manage reports — Mastodon's `staff` shorthand.
async fn staff_role_ids(state: &AppState) -> Result<Vec<i64>, ApiError> {
    Ok(role::list(&state.pool)
        .await?
        .into_iter()
        .filter(|r| r.can(permission::MANAGE_REPORTS))
        .map(|r| r.id)
        .collect())
}

/// Runs the listing and renders the page with a keyset `Link` header.
async fn respond(
    state: &AppState,
    path: &str,
    page: &Pagination,
    filter: AdminAccountFilter,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let limit = filter.limit;
    let views = admin_account::list(&state.pool, &filter).await?;
    let mut out = Vec::with_capacity(views.len());
    for view in &views {
        out.push(admin_account_json(&state.pool, &state.config.domain, view).await?);
    }
    let headers = link_header(state, path, page, limit, &views);
    Ok((headers, Json(Value::Array(out))))
}

/// Mastodon's id-keyset `Link` header: `next` once a full page comes back,
/// `prev` whenever the page is non-empty.
fn link_header(
    state: &AppState,
    path: &str,
    page: &Pagination,
    limit: i64,
    views: &[AdminAccountView],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if views.is_empty() {
        return headers;
    }
    let domain = &state.config.domain;
    let ascending = page.min_id.is_some();
    // Ids arrive in cursor order; `next` walks past the last, `prev` before the
    // first, regardless of direction.
    let first = views.first().map(|v| v.account.id).unwrap_or_default();
    let last = views.last().map(|v| v.account.id).unwrap_or_default();
    let (newest, oldest) = if ascending {
        (first, last)
    } else {
        (last, first)
    };
    let mut links = Vec::new();
    if views.len() == usize::try_from(limit).unwrap_or(usize::MAX) {
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

// ---- Action verbs -------------------------------------------------------

/// The moderation actions `Admin::AccountAction` accepts. `none`/`disable` are
/// only meaningful for local accounts (Mastodon's `types_for_account`).
const ACTION_TYPES: [&str; 5] = ["none", "disable", "sensitive", "silence", "suspend"];

/// The decoded body of `POST …/accounts/{id}/action`.
struct AccountActionInput {
    action_type: String,
    text: String,
    report_id: Option<i64>,
}

/// A JSON id field that may arrive as a string or a number.
fn value_to_id(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn parse_action(headers: &HeaderMap, body: &[u8]) -> Result<AccountActionInput, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
        Ok(AccountActionInput {
            action_type: value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            report_id: value.get("report_id").and_then(value_to_id),
        })
    } else {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
        let mut input = AccountActionInput {
            action_type: String::new(),
            text: String::new(),
            report_id: None,
        };
        for (key, value) in pairs {
            match key.as_str() {
                "type" => input.action_type = value,
                "text" => input.text = value,
                "report_id" => input.report_id = value.parse().ok(),
                _ => {}
            }
        }
        Ok(input)
    }
}

/// `POST /api/v1/admin/accounts/{id}/action` — Mastodon's
/// `Api::V1::Admin::AccountActionsController#create` /
/// `Admin::AccountAction`. Applies the requested action, records a strike in
/// `account_warnings`, and (for `suspend` of a local account) federates a
/// blanked `Update(Actor)` so remotes mirror the reversible state. Responds
/// `{}` like `render_empty`.
pub async fn create_action(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let input = parse_action(&headers, &body)?;
    if !ACTION_TYPES.contains(&input.action_type.as_str()) {
        return Err(ApiError::Unprocessable(
            "Validation failed: Type is not included in the list".into(),
        ));
    }
    let target = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A cited report must exist, like Mastodon's `Report.find`.
    if let Some(report_id) = input.report_id
        && plamenu_db::report::find_by_id(&state.pool, report_id)
            .await?
            .is_none()
    {
        return Err(ApiError::NotFound);
    }

    crate::moderation::apply_account_action(
        &state,
        &admin.role,
        admin.current.account.id,
        &target,
        &input.action_type,
        &input.text,
        input.report_id,
    )
    .await?;

    Ok(Json(json!({})))
}

/// `POST /api/v1/admin/accounts/{id}/enable` — lift a `disable`.
pub async fn enable(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let view = require_local_account(&state, id).await?;
    user::set_disabled(&state.pool, view.account.id, false).await?;
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "enable",
        &admin_log::Target::user(&view.account),
    )
    .await?;
    render(&state, id).await
}

/// `POST /api/v1/admin/accounts/{id}/approve` — approve a pending sign-up.
pub async fn approve(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let view = require_local_account(&state, id).await?;
    if user::approve(&state.pool, view.account.id).await?
        && let Some(user) = user::find_by_account_id(&state.pool, view.account.id).await?
        && user.confirmed()
    {
        // Confirmed + now approved = functional (Mastodon's
        // `prepare_new_user!`): welcome mail, staff notifications and the
        // `account.approved` webhook. An unconfirmed approval runs these
        // later, when the user clicks their confirmation link.
        crate::registration::user_became_functional(&state, &user).await?;
    }
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "approve",
        &admin_log::Target::user(&view.account),
    )
    .await?;
    render(&state, id).await
}

/// `POST /api/v1/admin/accounts/{id}/reject` — reject and delete a pending
/// sign-up. Responds `{}` like `render_empty`.
pub async fn reject(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let view = require_local_account(&state, id).await?;
    // Rejecting deletes the pending account — same outrank/self gate as any
    // destructive verb (a pending sign-up is role-less, so staff always
    // outrank it, but the self and hierarchy invariants still apply).
    let target_role = crate::moderation::target_role(&state, &view.account).await?;
    crate::moderation::authorize_account_action(
        &admin.role,
        admin.current.account.id,
        view.account.id,
        target_role.as_ref(),
        crate::moderation::ActionKind::Reject,
    )?;
    crate::moderation::guard_last_administrator(
        &state,
        &view.account,
        target_role.as_ref(),
        crate::moderation::ActionKind::Reject,
    )
    .await?;
    // Deletion and audit line commit together (finding #49).
    admin_log::record_account_deletion(
        &state.pool,
        view.account.id,
        admin.current.account.id,
        "reject",
        &admin_log::Target::user(&view.account),
    )
    .await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/admin/accounts/{id}/unsuspend` — lift a suspension; a local
/// account is re-advertised to followers via `Update(Actor)`.
pub async fn unsuspend(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let target = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::moderation::unsuspend_account(&state, &target).await?;
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "unsuspend",
        &admin_log::Target::account(&target),
    )
    .await?;
    render(&state, id).await
}

/// `POST /api/v1/admin/accounts/{id}/unsilence`.
pub async fn unsilence(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let target = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    account::unsilence(&state.pool, target.id).await?;
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "unsilence",
        &admin_log::Target::account(&target),
    )
    .await?;
    render(&state, id).await
}

/// `POST /api/v1/admin/accounts/{id}/unsensitive`.
pub async fn unsensitive(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let target = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    account::unsensitize(&state.pool, target.id).await?;
    admin_log::record(
        &state.pool,
        admin.current.account.id,
        "unsensitive",
        &admin_log::Target::account(&target),
    )
    .await?;
    render(&state, id).await
}

/// `DELETE /api/v1/admin/accounts/{id}` — permanently delete the account.
/// Responds `{}` like `render_empty`.
pub async fn destroy(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    admin.require_resource(permission::MANAGE_USERS, true, "accounts")?;
    let target = account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Permanent deletion outranks the target, is never applied to oneself, and
    // requires `DELETE_USER_DATA` on top of `MANAGE_USERS`.
    let target_role = crate::moderation::target_role(&state, &target).await?;
    crate::moderation::authorize_account_action(
        &admin.role,
        admin.current.account.id,
        target.id,
        target_role.as_ref(),
        crate::moderation::ActionKind::Destroy,
    )?;
    // …and never the last administrator.
    crate::moderation::guard_last_administrator(
        &state,
        &target,
        target_role.as_ref(),
        crate::moderation::ActionKind::Destroy,
    )
    .await?;
    crate::moderation::destroy_suspended_account(&state, admin.current.account.id, &target).await?;
    Ok(Json(json!({})))
}

/// Loads an account that must be local and have a user row, mirroring
/// Mastodon's `require_local_account!`: a missing account 404s, a remote one
/// (or a local one without a user) is forbidden.
async fn require_local_account(state: &AppState, id: i64) -> Result<AdminAccountView, ApiError> {
    let view = admin_account::show(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !view.account.is_local() || !view.has_user {
        return Err(ApiError::Forbidden(
            "This action is not allowed on this account".into(),
        ));
    }
    Ok(view)
}

/// Re-renders the `Admin::Account` serializer after an action, like Mastodon's
/// action verbs do.
async fn render(state: &AppState, id: i64) -> Result<Json<Value>, ApiError> {
    let view = admin_account::show(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        admin_account_json(&state.pool, &state.config.domain, &view).await?,
    ))
}
