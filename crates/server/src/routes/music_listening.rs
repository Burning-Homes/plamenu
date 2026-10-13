//! Listening sessions and history are authenticated and private.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_listening::{self, PreferenceChange, Progress, Start},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
fn response(value: impl serde::Serialize) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/music/listening/preferences",
            get(preferences).put(set_preferences),
        )
        .route("/api/v1/music/listening/sessions", post(start))
        .route("/api/v1/music/listening/sessions/{token}", post(progress))
        .route("/api/v1/music/history", get(history).delete(erase))
}
async fn preferences(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_listening::preferences(&state, current.account.id).await?,
    ))
}
async fn set_preferences(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: PreferenceChange = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_listening::set_preferences(&state, current.account.id, &input).await?,
    ))
}
async fn start(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Start = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_listening::start(&state, current.account.id, &input).await?,
    ))
}
async fn progress(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Progress = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_listening::progress(&state, current.account.id, &token, &input).await?,
    ))
}
#[derive(Default, Deserialize)]
struct Page {
    max_id: Option<i64>,
}
async fn history(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_listening::history(&state, current.account.id, page.max_id).await?,
    ))
}
async fn erase(State(state): State<AppState>, current: CurrentUser) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    music_listening::erase(&state, current.account.id).await?;
    Ok(response(serde_json::json!({})))
}
