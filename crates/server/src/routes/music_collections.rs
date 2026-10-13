//! Music collection CRUD and authorized playlist export.
use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    music_collections::{self, Add, New, Reorder},
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
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/collections", get(list).post(create))
        .route(
            "/api/v1/music/collections/{id}",
            get(show).put(update).delete(delete),
        )
        .route(
            "/api/v1/music/collections/{id}/grants/{member}",
            axum::routing::put(grant).delete(revoke),
        )
        .route(
            "/api/v1/music/collections/{id}/entries",
            axum::routing::post(add),
        )
        .route(
            "/api/v1/music/collections/{id}/entries/{entry}",
            axum::routing::delete(remove),
        )
        .route(
            "/api/v1/music/collections/{id}/order",
            axum::routing::put(reorder),
        )
        .route("/api/v1/music/collections/{id}/stats", get(stats))
        .route(
            "/api/v1/music/collections/{id}/tags",
            axum::routing::put(replace_tags).post(add_tags),
        )
        .route(
            "/api/v1/music/collections/{id}/tags/{tag}",
            axum::routing::delete(remove_tag),
        )
        .route(
            "/api/v1/music/collections/{id}/contents-sharing",
            axum::routing::put(contents_sharing),
        )
}
pub fn public_router() -> Router<AppState> {
    Router::new().route("/music/collections/{id}/export.m3u", get(export))
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
    kind: Option<String>,
    mine: Option<bool>,
    max_id: Option<i64>,
    after: Option<i64>,
}
async fn list(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let mut items = music_collections::search(
        &state,
        Some(current.account.id),
        q.mine.unwrap_or(false),
        q.max_id,
        q.q.as_deref().unwrap_or_default(),
        q.kind.as_deref().filter(|v| !v.is_empty()),
    )
    .await?;
    let before = if items.len() > 60 {
        items.truncate(60);
        items.last().map(|v| v.id.to_string())
    } else {
        None
    };
    Ok(response(
        json!({"items":items.iter().map(music_collections::Collection::entity).collect::<Vec<_>>(),"before":before}),
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
        music_collections::create(&state, current.account.id, &input)
            .await?
            .entity(),
    ))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let item = music_collections::get(&state, id, Some(current.account.id)).await?;
    let mut entity = item.entity();
    let (items, after) =
        music_collections::playable_page(&state, id, Some(current.account.id), q.after).await?;
    entity["entries"] = serde_json::to_value(items).map_err(|e| ApiError::Internal(Box::new(e)))?;
    entity["after"] = json!(after.map(|v| v.to_string()));
    Ok(response(entity))
}
async fn stats(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let counts = music_collections::statistics(&state, id, Some(current.account.id)).await?;
    Ok(response(
        serde_json::to_value(counts).map_err(|error| ApiError::Internal(Box::new(error)))?,
    ))
}
#[derive(Deserialize)]
struct Change {
    revision: String,
    #[serde(flatten)]
    attributes: New,
}
fn revision(input: &str) -> Result<i64, ApiError> {
    input
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid collection revision".into()))
}
async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Change = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_collections::update(
            &state,
            current.account.id,
            id,
            revision(&input.revision)?,
            &input.attributes,
        )
        .await?
        .entity(),
    ))
}
#[derive(Deserialize)]
struct Revision {
    revision: String,
}
#[derive(Deserialize)]
struct ContentsSharing {
    revision: String,
    share_tracks: bool,
}
async fn contents_sharing(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: ContentsSharing = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_collections::set_contents_sharing(
            &state,
            current.account.id,
            id,
            revision(&input.revision)?,
            input.share_tracks,
        )
        .await?
        .entity(),
    ))
}
async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    music_collections::delete(&state, current.account.id, id, revision(&input.revision)?).await?;
    Ok(response(json!({})))
}
async fn add(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Add = super::params::parse_body(&headers, &body)?;
    music_collections::add(&state, current.account.id, id, &input).await?;
    Ok(response(json!({})))
}
async fn remove(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, entry)): Path<(i64, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    music_collections::remove(
        &state,
        current.account.id,
        id,
        revision(&input.revision)?,
        entry,
    )
    .await?;
    Ok(response(json!({})))
}
async fn reorder(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Reorder = super::params::parse_body(&headers, &body)?;
    music_collections::reorder(&state, current.account.id, id, &input).await?;
    Ok(response(json!({})))
}
async fn export(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
) -> Result<Response, ApiError> {
    if let Some(user) = &token {
        user.require_scope("read:statuses")?;
    }
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    let body = music_collections::m3u(&state, id, viewer).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "audio/x-mpegurl; charset=utf-8"),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        [(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"playlist-{id}.m3u\""),
        )],
        body,
    )
        .into_response())
}

async fn grant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    music_collections::grant(
        &state,
        current.account.id,
        id,
        revision(&input.revision)?,
        member,
        true,
    )
    .await?;
    Ok(response(json!({})))
}
async fn revoke(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    music_collections::grant(
        &state,
        current.account.id,
        id,
        revision(&input.revision)?,
        member,
        false,
    )
    .await?;
    Ok(response(json!({})))
}

async fn replace_tags(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Json(input): Json<music_collections::Tags>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    Ok(response(
        music_collections::tags(
            &state,
            user.account.id,
            id,
            &input,
            music_collections::TagAction::Replace,
        )
        .await?
        .entity(),
    ))
}
async fn add_tags(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Json(input): Json<music_collections::Tags>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    Ok(response(
        music_collections::tags(
            &state,
            user.account.id,
            id,
            &input,
            music_collections::TagAction::Add,
        )
        .await?
        .entity(),
    ))
}
#[derive(Deserialize)]
struct TagRevision {
    revision: String,
}
async fn remove_tag(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, tag)): Path<(i64, String)>,
    Query(input): Query<TagRevision>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    Ok(response(
        music_collections::tags(
            &state,
            user.account.id,
            id,
            &music_collections::Tags {
                revision: input.revision,
                tags: Vec::new(),
            },
            music_collections::TagAction::Remove(tag),
        )
        .await?
        .entity(),
    ))
}
