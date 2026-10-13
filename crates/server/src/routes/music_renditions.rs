//! Music clients use negotiated encodes through the current asset ACL.
use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    music_renditions,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
#[derive(Deserialize)]
struct Options {
    format: String,
    bitrate: Option<u32>,
}
#[derive(Deserialize)]
struct SharedOptions {
    token: String,
    format: Option<String>,
    bitrate: Option<u32>,
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/assets/{asset}/renditions", post(create))
        .route(
            "/api/v1/music/assets/{asset}/renditions/{job}",
            get(show).delete(cancel),
        )
}
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/music/assets/{asset}/renditions/{job}/stream", get(stream))
        .route(
            "/music/share/assets/{asset}/renditions",
            post(shared_create),
        )
        .route(
            "/music/share/assets/{asset}/renditions/{job}",
            get(shared_show),
        )
        .route(
            "/music/share/assets/{asset}/renditions/{job}/stream",
            get(shared_stream),
        )
}
fn shared_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}
async fn shared_create(
    State(state): State<AppState>,
    Path(asset): Path<i64>,
    Query(options): Query<SharedOptions>,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
) -> Result<Response, ApiError> {
    let viewer = web.as_ref().map(|u| u.current.account.id);
    let scope = crate::music_shares::resolve(&state, &options.token, viewer).await?;
    let selection = music_renditions::negotiate(
        &state,
        viewer,
        options.format.as_deref().unwrap_or("mp3"),
        options.bitrate,
    )
    .await?;
    let row = music_renditions::request_shared(&state, asset, viewer, selection, &scope).await?;
    let mut entity = row.entity();
    entity["stream_url"] = serde_json::json!(format!(
        "/music/share/assets/{asset}/renditions/{}/stream?token={}",
        row.id, options.token
    ));
    Ok(shared_response(Json(entity).into_response()))
}
async fn shared_show(
    State(state): State<AppState>,
    Path((asset, job)): Path<(i64, i64)>,
    Query(options): Query<SharedOptions>,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
) -> Result<Response, ApiError> {
    let viewer = web.as_ref().map(|u| u.current.account.id);
    let scope = crate::music_shares::resolve(&state, &options.token, viewer).await?;
    let row = music_renditions::get_shared(&state, asset, job, viewer, &scope).await?;
    let mut entity = row.entity();
    entity["stream_url"] = serde_json::json!(format!(
        "/music/share/assets/{asset}/renditions/{job}/stream?token={}",
        options.token
    ));
    Ok(shared_response(Json(entity).into_response()))
}
async fn shared_stream(
    State(state): State<AppState>,
    Path((asset, job)): Path<(i64, i64)>,
    Query(options): Query<SharedOptions>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = &token {
        token.require_scope("read:statuses")?;
    }
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    let scope = crate::music_shares::resolve(&state, &options.token, viewer).await?;
    let row = music_renditions::get_shared(&state, asset, job, viewer, &scope).await?;
    if row.state != "ready" {
        return Err(ApiError::Conflict("Rendition is not ready".into()));
    }
    music_renditions::authorize_playback(&state, &row, viewer).await?;
    let response = serve_ready(
        &state,
        &row,
        &headers,
        crate::music_renditions::cold::Access {
            viewer,
            scope: Some(scope),
        },
    )
    .await?;
    let credential = token
        .as_ref()
        .or_else(|| web.as_ref().map(|u| &u.current))
        .map(crate::music_bytes::Credential::from_user);
    let response = crate::music_bytes::wrap(&state, response, None, credential).await?;
    Ok(shared_response(response))
}
fn response(row: &music_renditions::Rendition) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(row.entity()),
    )
        .into_response()
}
async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(asset): Path<i64>,
    Query(options): Query<Options>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let selection = music_renditions::negotiate(
        &state,
        Some(current.account.id),
        &options.format,
        options.bitrate,
    )
    .await?;
    let row = music_renditions::request(&state, asset, Some(current.account.id), selection).await?;
    Ok(response(&row))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((asset, job)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        &music_renditions::get(&state, asset, job, Some(current.account.id)).await?,
    ))
}
async fn cancel(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((asset, job)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_renditions::cancel(&state, asset, job, current.account.id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}
async fn stream(
    State(state): State<AppState>,
    Path((asset, job)): Path<(i64, i64)>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = &token {
        token.require_scope("read:statuses")?;
    }
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    let row = music_renditions::get(&state, asset, job, viewer).await?;
    if row.state != "ready" {
        return Err(ApiError::Conflict("Rendition is not ready".into()));
    }
    music_renditions::authorize_playback(&state, &row, viewer).await?;
    let response = serve_ready(
        &state,
        &row,
        &headers,
        crate::music_renditions::cold::Access {
            viewer,
            scope: None,
        },
    )
    .await?;
    let credential = token
        .as_ref()
        .or_else(|| web.as_ref().map(|u| &u.current))
        .map(crate::music_bytes::Credential::from_user);
    crate::music_bytes::wrap(&state, response, None, credential).await
}
pub(crate) async fn serve_ready(
    state: &AppState,
    row: &music_renditions::Rendition,
    headers: &HeaderMap,
    access: crate::music_renditions::cold::Access,
) -> Result<Response, ApiError> {
    let key = row.file_name.as_ref().ok_or(ApiError::NotFound)?;
    let format = music_renditions::Format::parse(&row.format)?;
    let mut response = super::media::stream_stored_file(
        state,
        key,
        format.content_type(),
        "private, no-store",
        headers,
    )
    .await?;
    response
        .headers_mut()
        .insert(header::VARY, "Authorization, Cookie".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    crate::music_bytes::wrap(
        state,
        response,
        Some(crate::music_bytes::Source::rendition(row, access)?),
        None,
    )
    .await
}
