//! Owner-only saved references, with current source metadata and scoped mutations.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_saved::{self, Reference},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
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
    Router::new()
        .route("/api/v1/music/mine", get(mine))
        .route("/api/v1/music/saved", get(index).post(save))
        .route("/api/v1/music/saved/{id}", axum::routing::delete(remove))
}
async fn mine(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(input): Query<crate::music_mine::Request>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        crate::music_mine::page(&state, current.account.id, &input).await?,
    ))
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
        music_saved::page(&state, current.account.id, page.max_id).await?,
    ))
}
async fn save(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Reference = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_saved::save(&state, current.account.id, &input).await?,
    ))
}
async fn remove(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    music_saved::remove(&state, current.account.id, id).await?;
    Ok(response(serde_json::json!({})))
}
