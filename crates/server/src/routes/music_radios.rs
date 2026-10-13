//! Station configuration API, with private owner-only settings.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_radios::{self, New},
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
        .route("/api/v1/music/radios", get(index).post(create))
        .route(
            "/api/v1/music/radios/{id}",
            get(show).put(update).delete(delete),
        )
        .route(
            "/api/v1/music/radios/{id}/grants/{member}",
            axum::routing::put(grant).delete(revoke),
        )
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
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let mut items = music_radios::search(
        &state,
        Some(current.account.id),
        page.mine.unwrap_or(false),
        page.max_id,
        page.q.as_deref().unwrap_or_default(),
    )
    .await?;
    let before = if items.len() > 50 {
        items.truncate(50);
        items.last().map(|v| v.id.to_string())
    } else {
        None
    };
    Ok(response(
        json!({"items":items.iter().map(|r|r.entity(Some(current.account.id))).collect::<Vec<_>>(),"before":before}),
    ))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_radios::get(&state, id, Some(current.account.id))
            .await?
            .entity(Some(current.account.id)),
    ))
}
async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: New = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_radios::create(&state, current.account.id, &input)
            .await?
            .entity(Some(current.account.id)),
    ))
}
#[derive(Deserialize)]
struct Change {
    revision: String,
    #[serde(flatten)]
    fields: New,
}
#[derive(Deserialize)]
struct Revision {
    revision: String,
}
fn revision(text: &str) -> Result<i64, ApiError> {
    text.parse()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid radio revision".into()))
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
        music_radios::update(
            &state,
            current.account.id,
            id,
            revision(&input.revision)?,
            &input.fields,
        )
        .await?
        .entity(Some(current.account.id)),
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
    music_radios::delete(&state, current.account.id, id, revision(&input.revision)?).await?;
    Ok(response(json!({})))
}
async fn grant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    music_radios::grant(&state, current.account.id, id, member, true).await?;
    Ok(response(json!({})))
}
async fn revoke(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    music_radios::grant(&state, current.account.id, id, member, false).await?;
    Ok(response(json!({})))
}
