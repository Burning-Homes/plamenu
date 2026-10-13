//! Music-client keys never authenticate this management API.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_clients::{self, Request},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
fn response(value: impl serde::Serialize) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Json(value),
    )
        .into_response()
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/clients", get(index).post(create))
        .route("/api/v1/music/clients/{id}", axum::routing::delete(revoke))
}
async fn index(State(state): State<AppState>, user: CurrentUser) -> Result<Response, ApiError> {
    user.require_scope("read:accounts")?;
    Ok(response(
        music_clients::list(&state, user.account.id).await?,
    ))
}
async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(input): Json<Request>,
) -> Result<Response, ApiError> {
    user.require_scope("write:accounts")?;
    user.require_scope("read:statuses")?;
    if input.can_manage {
        user.require_scope("write:statuses")?;
    }
    Ok(response(
        music_clients::create(&state, user.account.id, Some(user.token_id), &input).await?,
    ))
}
async fn revoke(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(key): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("write:accounts")?;
    music_clients::revoke(&state, user.account.id, key).await?;
    Ok(response(serde_json::json!({})))
}
