//! Music download APIs retain existing OAuth music scopes and native sessions.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_downloads};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[derive(Default, serde::Deserialize)]
struct Page {
    before: Option<i64>,
}
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
        .route("/api/v1/music/downloads", get(index).post(create))
        .route("/api/v1/music/downloads/{id}", get(show).delete(delete))
        .route("/api/v1/music/downloads/{id}/cancel", post(cancel))
}
pub fn file_router() -> Router<AppState> {
    Router::new().route("/music/downloads/{id}/file", get(file))
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let rows = music_downloads::list(&state, current.account.id, page.before).await?;
    let mut values = Vec::new();
    for row in rows {
        values.push(row.entity(&state).await?);
    }
    Ok(response(values))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_downloads::get(&state, current.account.id, id)
            .await?
            .entity(&state)
            .await?,
    ))
}
async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_downloads::create(&state, current.account.id, &input)
            .await?
            .entity(&state)
            .await?,
    ))
}
#[derive(serde::Deserialize)]
struct Revision {
    revision: String,
}
async fn mutate(
    state: &AppState,
    current: &CurrentUser,
    id: i64,
    headers: &HeaderMap,
    body: &Bytes,
    cancel: bool,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(headers, body)?;
    let revision = input
        .revision
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid music download revision".into()))?;
    music_downloads::remove(state, current.account.id, id, revision, cancel).await?;
    Ok(response(serde_json::json!({})))
}
async fn cancel(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    mutate(&state, &current, id, &headers, &body, true).await
}
async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    mutate(&state, &current, id, &headers, &body, false).await
}
async fn file(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    crate::auth::MaybeUser(token): crate::auth::MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let current = token
        .or_else(|| web.map(|web| web.current))
        .ok_or_else(|| ApiError::Unauthorized("Sign in to download music".into()))?;
    current.require_scope("read:statuses")?;
    music_downloads::file::serve(&state, current.account.id, id, current.token_id, &headers).await
}
