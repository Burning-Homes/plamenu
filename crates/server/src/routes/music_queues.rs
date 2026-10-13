//! Authenticated account/device playback queues.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_queues::{self, Change},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
fn response(queue: music_queues::Queue) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(queue),
    )
        .into_response()
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/queues", get(devices))
        .route(
            "/api/v1/music/queues/{device}",
            get(show).post(change).delete(forget),
        )
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(device): Path<String>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_queues::get(&state, current.account.id, &device).await?,
    ))
}
async fn change(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(device): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Change = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_queues::change(&state, current.account.id, &device, &input).await?,
    ))
}

async fn devices(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(music_queues::devices(&state, current.account.id).await?),
    )
        .into_response())
}
#[derive(serde::Deserialize)]
struct Revision {
    revision: String,
}
async fn forget(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(device): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_queues::change(
            &state,
            current.account.id,
            &device,
            &Change {
                revision: input.revision,
                action: music_queues::Action::Forget,
            },
        )
        .await?,
    ))
}
