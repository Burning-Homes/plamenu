//! Native metadata and ordered track API for reusable music resources.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_entities::{self, Kind},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
fn kind(path: &str) -> Result<Kind, ApiError> {
    match path {
        "artists" => Ok(Kind::Artist),
        "albums" => Ok(Kind::Album),
        _ => Err(ApiError::NotFound),
    }
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/{kind}", get(index).post(create))
        .route(
            "/api/v1/music/{kind}/{id}",
            get(show).put(edit).delete(delete),
        )
        .route("/api/v1/music/{kind}/{id}/tracks", get(tracks))
        .route("/api/v1/music/{kind}/{id}/stats", get(stats))
        .route(
            "/api/v1/music/albums/{id}/access",
            axum::routing::put(access),
        )
        .route("/api/v1/music/albums/{id}/grants", get(members))
        .route(
            "/api/v1/music/albums/{id}/grants/{member}",
            axum::routing::put(grant).delete(revoke),
        )
}
#[derive(Default, Deserialize)]
struct MemberPage {
    max_id: Option<i64>,
}
async fn members(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Query(page): Query<MemberPage>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        serde_json::to_value(
            crate::music_album_access::members(&state, current.account.id, id, page.max_id).await?,
        )
        .map_err(|error| ApiError::Internal(Box::new(error)))?,
    ))
}
async fn grant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    crate::music_album_access::grant(&state, current.account.id, id, member, true).await?;
    Ok(response(json!({})))
}
async fn revoke(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    crate::music_album_access::grant(&state, current.account.id, id, member, false).await?;
    Ok(response(json!({})))
}
async fn access(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(response(
        crate::music_album_access::update(&state, current.account.id, id, &input)
            .await?
            .entity(Kind::Album),
    ))
}
fn response(value: Value) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
#[derive(Default, Deserialize)]
struct Page {
    q: Option<String>,
    mine: Option<bool>,
    max_id: Option<i64>,
    after: Option<i64>,
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(path): Path<String>,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let kind = kind(&path)?;
    let mut items = music_entities::catalogue(
        &state,
        kind,
        Some(current.account.id),
        q.mine.unwrap_or(false),
        q.q.as_deref().unwrap_or_default(),
        q.max_id,
    )
    .await?;
    let before = if items.len() > 60 {
        items.truncate(60);
        items.last().map(|e| e.id.to_string())
    } else {
        None
    };
    Ok(response(
        json!({"items":items.iter().map(|e|e.entity(kind)).collect::<Vec<_>>(),"before":before}),
    ))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((path, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let kind = kind(&path)?;
    Ok(response(
        music_entities::get(&state, kind, id, Some(current.account.id))
            .await?
            .entity(kind),
    ))
}
async fn tracks(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((path, id)): Path<(String, i64)>,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let page =
        music_entities::tracks(&state, kind(&path)?, id, Some(current.account.id), q.after).await?;
    Ok(response(
        json!({"items":page.items.iter().map(crate::music_assets::Asset::entity).collect::<Vec<_>>(),"after":page.after.map(|id|id.to_string())}),
    ))
}
async fn stats(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((path, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let counts =
        music_entities::statistics(&state, kind(&path)?, id, Some(current.account.id)).await?;
    Ok(response(
        serde_json::to_value(counts).map_err(|error| ApiError::Internal(Box::new(error)))?,
    ))
}
async fn edit(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((path, id)): Path<(String, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let kind = kind(&path)?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_entities::update(&state, current.account.id, kind, id, &input)
            .await?
            .entity(kind),
    ))
}

async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let kind = kind(&path)?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_entities::create(&state, current.account.id, kind, &input)
            .await?
            .entity(kind),
    ))
}

async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((path, id)): Path<(String, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input = super::params::parse_body(&headers, &body)?;
    music_entities::delete(&state, current.account.id, kind(&path)?, id, &input).await?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}
