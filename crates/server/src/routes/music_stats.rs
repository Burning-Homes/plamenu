//! Listening charts are private and do not count requests for bytes.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_stats};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/music/stats", get(index))
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(input): Query<music_stats::Request>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(music_stats::report(&state, current.account.id, &input).await?),
    )
        .into_response())
}
