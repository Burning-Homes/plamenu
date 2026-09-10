//! `/api/v1/polls` — viewing polls and voting.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use plamenu_db::poll::{self, Poll};
use plamenu_db::{account, status};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{can_view, poll_json};
use crate::error::ApiError;
use crate::state::AppState;

/// Loads a poll and enforces its status' visibility for `viewer` — an
/// invisible poll is a 404, like Mastodon's `authorize @poll.status, :show?`.
async fn visible_poll(
    state: &AppState,
    poll_id: i64,
    viewer: Option<i64>,
) -> Result<(Poll, status::Status), ApiError> {
    let item = poll::find_by_id(&state.pool, poll_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let stored = status::find_by_id(&state.pool, item.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &stored, viewer).await? {
        return Err(ApiError::NotFound);
    }
    Ok((item, stored))
}

async fn render_poll(
    state: &AppState,
    item: &Poll,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let own_votes = match viewer {
        Some(viewer_id) => poll::votes_by(&state.pool, item.id, viewer_id).await?,
        None => Vec::new(),
    };
    let author_domain = account::find_by_id(&state.pool, item.account_id)
        .await?
        .and_then(|author| author.domain);
    let allow_direct = crate::entities::allow_direct_media(&state.pool, viewer).await;
    let emojis = crate::entities::poll_emojis_json(
        &state.pool,
        &state.config.domain,
        author_domain.as_deref(),
        item,
        allow_direct,
    )
    .await?;
    poll_json(item, viewer, &own_votes, &emojis)
}

/// `GET /api/v1/polls/{id}` — public polls need no auth, like Mastodon.
pub async fn show(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(poll_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:statuses")?;
    }
    let viewer_id = viewer.map(|v| v.account.id);
    let (item, _) = visible_poll(&state, poll_id, viewer_id).await?;
    Ok(Json(render_poll(&state, &item, viewer_id).await?))
}

#[derive(Deserialize)]
struct VoteParams {
    #[serde(default)]
    choices: Vec<Value>,
}

/// The vote body: JSON `{"choices": ["0", 2]}`, or form `choices[]=0`.
fn parse_choices(headers: &HeaderMap, body: &[u8]) -> Result<Vec<Value>, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        let params: VoteParams = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")))?;
        return Ok(params.choices);
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    Ok(pairs
        .into_iter()
        .filter(|(key, _)| key == "choices[]" || key == "choices")
        .map(|(_, value)| Value::String(value))
        .collect())
}

/// `POST /api/v1/polls/{id}/votes` — records the viewer's choices and, for
/// remote polls, federates each vote to the poll's author.
pub async fn vote(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(poll_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let voter = &current.account;
    let raw = parse_choices(&headers, &body)?;
    let item = crate::polls::cast_vote(&state, voter, poll_id, &raw).await?;
    plamenu_db::metrics::record_interaction(&state.pool)
        .await
        .ok();
    Ok(Json(render_poll(&state, &item, Some(voter.id)).await?))
}
