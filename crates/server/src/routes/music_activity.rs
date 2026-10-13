//! Read-only resource activities, with current containing-resource authorization.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_activity};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::json;
pub async fn index(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((kind, id)): Path<(String, i64)>,
    Query(q): Query<music_activity::Query>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    let target = music_activity::resolve(&state, &kind, id, Some(user.account.id)).await?;
    let page = music_activity::page(&state, &target, Some(user.account.id), &q).await?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(json!({"items":page.items,"before":page.before.map(|id|id.to_string())})),
    )
        .into_response())
}
