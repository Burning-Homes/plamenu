//! Explicit remote music lookup exposes only the currently permitted cache.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_remote};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
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
        .route("/api/v1/music/remote/import", post(import))
        .route("/api/v1/music/remote/{id}", get(show))
        .route("/music/remote/{id}/stream", get(stream))
        .route("/music/remote/{id}/artwork", get(artwork))
        .route("/api/v1/music/remote/{id}/contents", get(contents))
}
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route(
            "/music/remote/{id}/public/{collection}/{entry}/stream",
            get(public_stream).options(crate::web::music_embeds::media_preflight),
        )
        .route(
            "/music/remote/{id}/public/{collection}/{entry}/artwork",
            get(public_artwork).options(crate::web::music_embeds::media_preflight),
        )
        .route(
            "/music/share/remote/{id}/{entry}/stream",
            get(shared_stream),
        )
        .route(
            "/music/share/remote/{id}/{entry}/artwork",
            get(shared_artwork),
        )
}

#[derive(serde::Deserialize)]
struct Import {
    url: String,
}
async fn import(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Import = super::params::parse_body(&headers, &body)?;
    let row = music_remote::import(&state, current.account.id, &input.url).await?;
    Ok(response(row.view()))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let row = music_remote::get(&state, id, current.account.id).await?;
    music_remote::catalogue::current(&state, &row).await?;
    Ok(response(row.view()))
}

#[derive(Default, serde::Deserialize)]
struct Page {
    before: Option<i64>,
    offset: Option<usize>,
}
async fn contents(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let root = music_remote::get(&state, id, current.account.id).await?;
    if let Some((items, more)) = music_remote::catalogue::children(
        &state,
        current.account.id,
        &root,
        page.offset.unwrap_or_default(),
    )
    .await?
    {
        return Ok(response(
            serde_json::json!({"items":items.iter().map(music_remote::Resource::view).collect::<Vec<_>>(),"before":null,"next_offset":more.then_some(page.offset.unwrap_or_default()+music_remote::catalogue::PAGE_SIZE)}),
        ));
    }
    let page = music_remote::contents(&state, id, current.account.id, page.before).await?;
    Ok(response(
        serde_json::json!({"items":page.items.iter().map(music_remote::Resource::view).collect::<Vec<_>>(),"before":page.before.map(|id|id.to_string())}),
    ))
}

async fn stream(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    crate::auth::MaybeUser(token): crate::auth::MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let current = token
        .or_else(|| web.map(|user| user.current))
        .ok_or_else(|| ApiError::Unauthorized("Sign in to play remote music".into()))?;
    current.require_scope("read:statuses")?;
    music_remote::stream::serve(
        &state,
        id,
        music_remote::stream::Access::Account {
            viewer: current.account.id,
            token: current.token_id,
        },
        &headers,
    )
    .await
}

async fn artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    crate::auth::MaybeUser(token): crate::auth::MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
) -> Result<Response, ApiError> {
    let current = token
        .or_else(|| web.map(|user| user.current))
        .ok_or_else(|| ApiError::Unauthorized("Sign in to view remote music".into()))?;
    current.require_scope("read:statuses")?;
    music_remote::artwork::serve(
        &state,
        id,
        music_remote::stream::Access::Account {
            viewer: current.account.id,
            token: current.token_id,
        },
    )
    .await
}

async fn public_stream(
    State(state): State<AppState>,
    Path((id, collection, entry)): Path<(i64, i64, i64)>,
    headers: HeaderMap,
) -> Response {
    if collection <= 0 || entry <= 0 {
        return crate::web::music_embeds::media_response(ApiError::NotFound.into_response());
    }
    music_remote::stream::serve(
        &state,
        id,
        music_remote::stream::Access::PublicCollection { collection, entry },
        &headers,
    )
    .await
    .map_or_else(
        |error| crate::web::music_embeds::media_response(error.into_response()),
        crate::web::music_embeds::media_response,
    )
}
async fn public_artwork(
    State(state): State<AppState>,
    Path((id, collection, entry)): Path<(i64, i64, i64)>,
) -> Response {
    if collection <= 0 || entry <= 0 {
        return crate::web::music_embeds::media_response(ApiError::NotFound.into_response());
    }
    music_remote::artwork::serve(
        &state,
        id,
        music_remote::stream::Access::PublicCollection { collection, entry },
    )
    .await
    .map_or_else(
        |error| crate::web::music_embeds::media_response(error.into_response()),
        crate::web::music_embeds::media_response,
    )
}

async fn shared_access(
    state: &AppState,
    token: &str,
    entry: i64,
) -> Result<music_remote::stream::Access, ApiError> {
    if entry <= 0 {
        return Err(ApiError::NotFound);
    }
    let scope = crate::music_shares::resolve(state, token, None).await?;
    if !matches!(scope.kind.as_str(), "library" | "playlist") {
        return Err(ApiError::NotFound);
    }
    Ok(music_remote::stream::Access::ShareCollection {
        link: scope.id,
        collection: scope.target_id,
        entry,
    })
}
async fn shared_stream(
    State(state): State<AppState>,
    Path((id, entry)): Path<(i64, i64)>,
    Query(input): Query<crate::routes::music_shares::Access>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let access = shared_access(&state, &input.token, entry).await?;
    music_remote::stream::serve(&state, id, access, &headers).await
}
async fn shared_artwork(
    State(state): State<AppState>,
    Path((id, entry)): Path<(i64, i64)>,
    Query(input): Query<crate::routes::music_shares::Access>,
) -> Result<Response, ApiError> {
    let access = shared_access(&state, &input.token, entry).await?;
    music_remote::artwork::serve(&state, id, access).await
}
