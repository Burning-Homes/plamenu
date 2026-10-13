//! Authenticated current-audience mention archive survives notification clear.
use crate::{AppState, auth::CurrentUser, error::ApiError, mentions};
use axum::{
    Json,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::json;
pub async fn index(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<mentions::Query>,
) -> Result<Response, ApiError> {
    user.require_scope("read:notifications")?;
    let page = mentions::page(&state, user.account.id, &q).await?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(json!({"items":page.items,"before":page.before.map(|id|id.to_string())})),
    )
        .into_response())
}
