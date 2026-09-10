//! `/api/v1/conversations` — direct-message threads, one entry per
//! conversation with the participants, the newest message and read state.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use plamenu_db::conversation;
use plamenu_db::conversation::AccountConversation;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::CurrentUser;
use crate::entities::{render_conversation, render_conversations};
use crate::error::ApiError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct ConversationParams {
    pub max_id: Option<i64>,
    pub since_id: Option<i64>,
    pub min_id: Option<i64>,
    pub limit: Option<i64>,
}

async fn conversation_json(
    state: &AppState,
    viewer_id: i64,
    row: &AccountConversation,
) -> Result<Value, ApiError> {
    render_conversation(&state.pool, &state.config.domain, viewer_id, row).await
}

/// Keyset pagination over `last_status_id`, the way Mastodon paginates
/// conversations.
fn link_header(domain: &str, limit: i64, page: &[AccountConversation]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut parts = Vec::new();
    if page.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = page.last().and_then(|row| row.last_status_id)
    {
        parts.push(format!(
            "<https://{domain}/api/v1/conversations?limit={limit}&max_id={last}>; rel=\"next\""
        ));
    }
    if let Some(first) = page.first().and_then(|row| row.last_status_id) {
        parts.push(format!(
            "<https://{domain}/api/v1/conversations?limit={limit}&min_id={first}>; rel=\"prev\""
        ));
    }
    if !parts.is_empty()
        && let Ok(value) = parts.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    headers
}

/// `GET /api/v1/conversations`.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(params): Query<ConversationParams>,
) -> Result<impl IntoResponse, ApiError> {
    current.require_scope("read:statuses")?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = conversation::list(
        &state.pool,
        current.account.id,
        params.max_id,
        params.since_id,
        params.min_id,
        limit,
    )
    .await?;
    let entities =
        render_conversations(&state.pool, &state.config.domain, current.account.id, &rows).await?;
    let headers = link_header(&state.config.domain, limit, &rows);
    Ok((headers, Json(Value::Array(entities))))
}

/// `POST /api/v1/conversations/{id}/read`.
pub async fn read(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(row_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    set_read_state(&state, &current, row_id, false).await
}

/// `POST /api/v1/conversations/{id}/unread`.
pub async fn unread(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(row_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    set_read_state(&state, &current, row_id, true).await
}

async fn set_read_state(
    state: &AppState,
    current: &CurrentUser,
    row_id: i64,
    unread: bool,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:conversations")?;
    let row = conversation::set_unread(&state.pool, current.account.id, row_id, unread)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        conversation_json(state, current.account.id, &row).await?,
    ))
}

/// `DELETE /api/v1/conversations/{id}` — drops the caller's view of the
/// conversation; a missing row is a 404, like Mastodon's `find`.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(row_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:conversations")?;
    if !conversation::delete_account_conversation(&state.pool, current.account.id, row_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(serde_json::json!({})))
}
