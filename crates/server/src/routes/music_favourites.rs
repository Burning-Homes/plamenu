//! Authenticated music saves preserve source identity and send no remote Like.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_favourites, music_queues::Source};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
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
    Router::new().route(
        "/api/v1/music/favourites",
        get(index).post(save).delete(remove),
    )
}
#[derive(Default, serde::Deserialize)]
struct Page {
    max_id: Option<i64>,
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_favourites::page(&state, current.account.id, page.max_id).await?,
    ))
}
async fn save(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Source = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_favourites::save(&state, current.account.id, &input).await?,
    ))
}
async fn remove(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Source = super::params::parse_body(&headers, &body)?;
    music_favourites::remove(&state, current.account.id, &input).await?;
    Ok(response(serde_json::json!({})))
}
