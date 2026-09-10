//! `/api/v1/lists/*`, `/api/v1/timelines/list/{id}` and
//! `/api/v1/accounts/{id}/lists` — user lists, Mastodon's model: private
//! groupings of followed accounts, each with its own timeline.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use plamenu_db::list::{self, AddMemberError, List};
use plamenu_db::{account, status};
use serde::Deserialize;
use serde_json::{Value, json};

use super::accounts_api::render_account_page;
use super::params::{parse_body, truthy};
use super::timelines::link_header;
use crate::auth::CurrentUser;
use crate::entities::{render_accounts, render_statuses};
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_STATUSES_LIMIT: i64 = 20;
const MAX_STATUSES_LIMIT: i64 = 40;
const DEFAULT_ACCOUNTS_LIMIT: i64 = 40;
const MAX_ACCOUNTS_LIMIT: i64 = 80;
/// Ceiling on the Mastodon `limit=0` "whole list" response. List membership
/// itself is unbounded, so honoring `limit=0` literally would let one large
/// list materialize every id, fetch and render every account, and build one
/// giant JSON array. We return everyone up to this cap and, when
/// the list is larger, hand back a `next` Link so the client keeps paging like
/// any bounded request. Generous enough that real curated lists come back whole.
const FULL_LIST_LIMIT: i64 = 500;
/// Distinct `account_ids` accepted per add/remove request. Generous for honest
/// list-building yet bounds the serial per-entry membership transaction so one
/// request cannot amplify into an unbounded query chain.
const MAX_LIST_ACCOUNTS_PER_REQUEST: usize = 100;

fn list_json(list: &List) -> Value {
    json!({
        "id": list.id.to_string(),
        "title": list.title,
        "replies_policy": list.replies_policy,
        "exclusive": list.exclusive,
    })
}

/// Validates a list's merged attributes the way Rails runs Mastodon's
/// `List` validations, collecting messages in declaration order into one
/// `RecordInvalid` wording.
fn validate_list(title: &str, replies_policy: &str, over_limit: bool) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if !list::REPLIES_POLICIES.contains(&replies_policy) {
        errors.push("Replies policy is not included in the list".to_owned());
    }
    if title.trim().is_empty() {
        errors.push("Title can't be blank".to_owned());
    } else if title.chars().count() > list::TITLE_LENGTH_LIMIT {
        errors.push(format!(
            "Title is too long (maximum is {} characters)",
            list::TITLE_LENGTH_LIMIT
        ));
    }
    if over_limit {
        errors.push("You have reached the maximum number of lists".to_owned());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

#[derive(Deserialize, Default)]
pub struct ListParams {
    title: Option<String>,
    replies_policy: Option<String>,
    /// JSON clients send a boolean, form clients a string.
    exclusive: Option<Value>,
}

fn bool_param(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => Some(truthy(Some(s))),
        _ => None,
    }
}

async fn owned_list(state: &AppState, owner: i64, list_id: i64) -> Result<List, ApiError> {
    list::find_owned(&state.pool, owner, list_id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// `GET /api/v1/lists`.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:lists")?;
    let lists = list::owned_by(&state.pool, current.account.id).await?;
    Ok(Json(Value::Array(lists.iter().map(list_json).collect())))
}

/// `POST /api/v1/lists`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:lists")?;
    let params: ListParams = parse_body(&headers, &body)?;
    let title = params.title.unwrap_or_default();
    let replies_policy = params.replies_policy.unwrap_or_else(|| "list".to_owned());
    let exclusive = bool_param(params.exclusive.as_ref()).unwrap_or(false);
    let over_limit =
        list::count_owned(&state.pool, current.account.id).await? >= list::PER_ACCOUNT_LIMIT;
    validate_list(&title, &replies_policy, over_limit)?;
    let created = list::create(
        &state.pool,
        current.account.id,
        &title,
        &replies_policy,
        exclusive,
    )
    .await?;
    Ok(Json(list_json(&created)))
}

/// `GET /api/v1/lists/{id}`.
pub async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:lists")?;
    let list = owned_list(&state, current.account.id, list_id).await?;
    Ok(Json(list_json(&list)))
}

/// `PUT /api/v1/lists/{id}` — absent attributes keep their value, like
/// Rails' permitted-params `update!`.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:lists")?;
    let existing = owned_list(&state, current.account.id, list_id).await?;
    let params: ListParams = parse_body(&headers, &body)?;
    let title = params.title.unwrap_or(existing.title);
    let replies_policy = params.replies_policy.unwrap_or(existing.replies_policy);
    let exclusive = bool_param(params.exclusive.as_ref()).unwrap_or(existing.exclusive);
    validate_list(&title, &replies_policy, false)?;
    let updated = list::update(&state.pool, list_id, &title, &replies_policy, exclusive).await?;
    Ok(Json(list_json(&updated)))
}

/// `DELETE /api/v1/lists/{id}`.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:lists")?;
    if !list::delete(&state.pool, current.account.id, list_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
pub struct AccountsQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/lists/{id}/accounts` — members newest-account-first,
/// paginated by account id; `limit=0` lists everyone like Mastodon, but capped
/// at [`FULL_LIST_LIMIT`] with a `next` Link beyond it.
pub async fn accounts_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
    Query(query): Query<AccountsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:lists")?;
    owned_list(&state, current.account.id, list_id).await?;
    let path = format!("/api/v1/lists/{list_id}/accounts");
    if query.limit == Some(0) {
        // Mastodon's "whole list" request — honored, but never unbounded: cap at
        // `FULL_LIST_LIMIT` so one large list can't blow up memory.
        let ids =
            list::members_page(&state.pool, list_id, None, None, Some(FULL_LIST_LIMIT)).await?;
        let mut members = account::find_by_ids(&state.pool, &ids).await?;
        if members.len() != ids.len() {
            return Err(ApiError::NotFound);
        }
        // Restore the query's newest-account-first order in O(n): a linear
        // `position` scan per element made this path quadratic in the member
        // count.
        let rank: std::collections::HashMap<i64, usize> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();
        members.sort_by_key(|member| rank.get(&member.id).copied().unwrap_or(usize::MAX));
        let accounts = render_accounts(
            &state.pool,
            &state.config.domain,
            &members,
            Some(current.account.id),
        )
        .await?;
        // When the list overflows the cap, point the client at the next page so
        // it can retrieve the rest with ordinary keyset pagination.
        let mut headers = HeaderMap::new();
        let cap = usize::try_from(FULL_LIST_LIMIT).unwrap_or(usize::MAX);
        if ids.len() == cap
            && let Some(last_id) = ids.last()
            && let Ok(value) = format!(
                "<https://{}{path}?limit={MAX_ACCOUNTS_LIMIT}&max_id={last_id}>; rel=\"next\"",
                state.config.domain
            )
            .parse()
        {
            headers.insert(axum::http::header::LINK, value);
        }
        return Ok((headers, Json(Value::Array(accounts))));
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_ACCOUNTS_LIMIT)
        .clamp(1, MAX_ACCOUNTS_LIMIT);
    let ids = list::members_page(
        &state.pool,
        list_id,
        query.max_id,
        query.since_id,
        Some(limit),
    )
    .await?;
    let pairs: Vec<(i64, i64)> = ids.iter().map(|&id| (id, id)).collect();
    render_account_page(&state, &path, limit, &pairs, Some(current.account.id)).await
}

/// `account_ids` from a JSON array or repeated `account_ids[]` form keys.
/// Unparseable ids 404 like `Account.find` would.
fn account_ids_param(headers: &HeaderMap, body: &[u8]) -> Result<Vec<i64>, ApiError> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mut ids = Vec::new();
    if content_type.starts_with("application/json") {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
        for entry in value
            .get("account_ids")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let id = match entry {
                Value::String(s) => s.parse().map_err(|_| ApiError::NotFound)?,
                Value::Number(n) => n.as_i64().ok_or(ApiError::NotFound)?,
                _ => return Err(ApiError::NotFound),
            };
            ids.push(id);
        }
    } else {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
        for (key, value) in pairs {
            if key == "account_ids[]" || key == "account_ids" {
                ids.push(value.parse().map_err(|_| ApiError::NotFound)?);
            }
        }
    }
    // Bound + deduplicate before the per-id existence probe and the per-entry
    // membership transaction: a repeated or oversized array
    // would otherwise force a large serial `find_by_id` chain and hold a
    // write transaction over that many round trips. Deduplication also matches
    // Mastodon resolving `account_ids` through `Account.where(id:)`.
    super::params::bounded_unique_ids(ids, MAX_LIST_ACCOUNTS_PER_REQUEST)
}

/// `POST /api/v1/lists/{id}/accounts` — every account must exist (404) and
/// be followed by the owner, a pending request, or the owner themself; the
/// batch is atomic, like Mastodon's transactional service.
pub async fn accounts_add(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:lists")?;
    owned_list(&state, current.account.id, list_id).await?;
    let ids = account_ids_param(&headers, &body)?;
    // Every cited account must exist (404), resolved in one query rather than a
    // `find_by_id` per id. `ids` is deduplicated, so a full match
    // means each requested account was found.
    if account::find_by_ids(&state.pool, &ids).await?.len() != ids.len() {
        return Err(ApiError::NotFound);
    }
    match list::add_members(&state.pool, list_id, current.account.id, &ids).await? {
        Ok(()) => Ok(Json(json!({}))),
        Err(AddMemberError::NotFollowed) => Err(ApiError::Unprocessable(
            "Validation failed: Account must be a followed account".into(),
        )),
        Err(AddMemberError::AlreadyMember) => Err(ApiError::Unprocessable(
            "Validation failed: Account has already been taken".into(),
        )),
    }
}

/// `DELETE /api/v1/lists/{id}/accounts` — absent memberships and unknown
/// ids are silently ignored, like Mastodon's `Account.where(id:)`.
pub async fn accounts_remove(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:lists")?;
    owned_list(&state, current.account.id, list_id).await?;
    let ids = account_ids_param(&headers, &body)?;
    list::remove_members(&state.pool, list_id, &ids).await?;
    Ok(Json(json!({})))
}

/// `GET /api/v1/accounts/{id}/lists` — the caller's own lists containing
/// the account.
pub async fn account_lists(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:lists")?;
    account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let lists = list::containing(&state.pool, current.account.id, account_id).await?;
    Ok(Json(Value::Array(lists.iter().map(list_json).collect())))
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    max_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/timelines/list/{id}`.
pub async fn timeline(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(list_id): Path<i64>,
    Query(query): Query<TimelineQuery>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:lists")?;
    let list = owned_list(&state, current.account.id, list_id).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_STATUSES_LIMIT)
        .clamp(1, MAX_STATUSES_LIMIT);
    let order = super::timelines::order_for(&state, Some(current.user.id)).await?;
    let statuses: Vec<status::Status> =
        list::timeline(&state.pool, &list, order, query.max_id, limit).await?;
    let entities = render_statuses(
        &state.pool,
        &state.config.domain,
        &statuses,
        Some(current.account.id),
    )
    .await?;
    let path = format!("/api/v1/timelines/list/{list_id}");
    let headers = link_header(&state.config.domain, &path, limit, &statuses);
    Ok((headers, Json(entities)))
}
