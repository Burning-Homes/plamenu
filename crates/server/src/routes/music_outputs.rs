//! Native HTTP output configuration uses existing music scopes and owner fences.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_outputs::{self, Command, New},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, Method, header},
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
        .route("/api/v1/music/outputs", get(index).post(create))
        .route(
            "/api/v1/music/outputs/{id}",
            get(show).put(update).delete(delete),
        )
        .route("/api/v1/music/outputs/{id}/command", post(command))
        .route("/api/v1/music/outputs/{id}/token", post(rotate))
}
#[derive(Default, Deserialize)]
struct Page {
    #[serde(default)]
    mine: bool,
    before: Option<i64>,
    #[serde(default)]
    q: String,
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(input): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let rows = music_outputs::list(
        &state,
        Some(current.account.id),
        input.mine,
        input.before,
        &input.q,
    )
    .await?;
    Ok(response(
        rows.iter()
            .map(|row| row.entity(Some(current.account.id)))
            .collect::<Vec<_>>(),
    ))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(response(
        music_outputs::get(&state, id, Some(current.account.id))
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
        music_outputs::create(&state, current.account.id, &input).await?,
    ))
}
#[derive(Deserialize)]
struct Revision {
    revision: String,
}
fn revision(value: &str) -> Result<i64, ApiError> {
    value
        .parse()
        .map_err(|_| ApiError::Unprocessable("Invalid HTTP stream revision".into()))
}
#[derive(Deserialize)]
struct Update {
    revision: String,
    #[serde(flatten)]
    input: New,
}
async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Update = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_outputs::update(
            &state,
            current.account.id,
            id,
            revision(&input.revision)?,
            &input.input,
        )
        .await?,
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
    music_outputs::delete(&state, current.account.id, id, revision(&input.revision)?).await?;
    Ok(response(serde_json::json!({})))
}
async fn command(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Command = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_outputs::command(&state, current.account.id, id, &input)
            .await?
            .entity(Some(current.account.id)),
    ))
}
async fn rotate(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Revision = super::params::parse_body(&headers, &body)?;
    Ok(response(
        music_outputs::rotate_token(&state, current.account.id, id, revision(&input.revision)?)
            .await?,
    ))
}

pub fn public_router() -> Router<AppState> {
    Router::new()
        .route(
            "/music/streams/{id}/audio",
            get(audio).head(audio).options(preflight),
        )
        .route(
            "/music/live/{mount}",
            get(mount_audio).head(mount_audio).options(preflight),
        )
}
#[derive(Default, Deserialize)]
struct Access {
    token: Option<String>,
}
async fn audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(input): Query<Access>,
    token: Result<crate::auth::MaybeUser, ApiError>,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, ApiError> {
    let query_capability = input.token.is_some();
    let supplied = input.token.or_else(|| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|raw| raw.len() <= 512)
            .map(str::to_owned)
    });
    let listener = if let Some(raw) = &supplied {
        music_outputs::authorize(&state, id, Some(raw))
            .await
            .is_ok()
    } else {
        false
    };
    let access = if query_capability || listener {
        music_outputs::stream::Access::Listener(supplied)
    } else if let Some(current) = token?.0.or_else(|| web.map(|web| web.current)) {
        current.require_scope("read:statuses")?;
        let row = music_outputs::get(&state, id, Some(current.account.id)).await?;
        if row.owner_id == current.account.id {
            music_outputs::stream::Access::Owner {
                viewer: current.account.id,
                token: current.token_id,
            }
        } else {
            music_outputs::stream::Access::Listener(None)
        }
    } else {
        music_outputs::stream::Access::Listener(None)
    };
    music_outputs::stream::serve_with(
        &state,
        id,
        access,
        headers
            .get("icy-metadata")
            .is_some_and(|value| value == "1"),
        method == Method::HEAD,
    )
    .await
}

async fn mount_audio(
    State(state): State<AppState>,
    Path(mount): Path<String>,
    Query(input): Query<Access>,
    token: Result<crate::auth::MaybeUser, ApiError>,
    web: crate::web::session::MaybeWebUser,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, ApiError> {
    let id: i64 = sqlx::query_scalar("SELECT id FROM music_outputs WHERE mount=$1")
        .bind(mount)
        .fetch_optional(&state.pool)
        .await
        .map_err(plamenu_db::DbError::from)?
        .ok_or(ApiError::NotFound)?;
    audio(
        State(state),
        Path(id),
        Query(input),
        token,
        web,
        headers,
        method,
    )
    .await
}

async fn preflight() -> Response {
    let mut response = crate::web::music_embeds::media_preflight().await;
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Range, Icy-MetaData, Authorization"
            .parse()
            .expect("static headers"),
    );
    response
}
