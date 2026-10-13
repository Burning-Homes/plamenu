//! Native music subscription API and Funkwhale library discovery.
use crate::{AppState, auth::CurrentUser, error::ApiError, music_follows};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/music/follows", get(list).post(change))
}

pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/ap/music/artists/{id}", get(artist))
        .route("/ap/music/albums/{id}", get(album))
        .route("/ap/music/artists/{id}/followers", get(artist_followers))
        .route("/ap/music/albums/{id}/followers", get(album_followers))
        .route("/ap/music/libraries/{id}", get(named_library))
        .route("/ap/music/libraries/{id}/followers", get(named_followers))
        .route("/users/{username}/library", get(library))
        .route("/ap/accounts/{username}/library", get(library))
        .route("/users/{username}/library/followers", get(followers))
        .route("/ap/accounts/{username}/library/followers", get(followers))
}

async fn list(State(state): State<AppState>, current: CurrentUser) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(music_follows::list(&state, current.account.id).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct Change {
    pub url: String,
    pub operation: String,
}

async fn change(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Change = super::params::parse_body(&headers, &body)?;
    match input.operation.as_str() {
        "follow" => music_follows::follow(&state, &current.account, &input.url).await?,
        "unfollow" => music_follows::unfollow(&state, &current.account, &input.url).await?,
        _ => return Err(ApiError::Unprocessable("Choose follow or unfollow".into())),
    }
    Ok(Json(json!({"ok":true})))
}

#[derive(Default, Deserialize)]
struct Page {
    page: Option<i64>,
}
async fn library(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let owner = super::local_actor_account(&state, &username, &uri).await?;
    if format!("https://{}{}", state.config.domain, uri.path())
        != music_follows::library_uri(&state.config.domain, &owner)
    {
        return Err(ApiError::NotFound);
    }
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(music_follows::library(&state, &owner, q.page).await?),
    )
        .into_response())
}

async fn followers(
    State(state): State<AppState>,
    Path(username): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let owner = super::local_actor_account(&state, &username, &uri).await?;
    let library = music_follows::library_uri(&state.config.domain, &owner);
    if owner.is_group()
        || !crate::instance_policy::account_id_visible(&state.pool, owner.id).await?
        || format!("https://{}{}", state.config.domain, uri.path())
            != format!("{library}/followers")
    {
        return Err(ApiError::NotFound);
    }
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM music_follows f JOIN accounts a ON a.id=f.follower_id WHERE f.target_uri=$1 AND f.local_target AND NOT f.pending AND a.suspended_at IS NULL")
        .bind(&library).fetch_one(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    Ok(([(header::CONTENT_TYPE,plamenu_ap::ACTIVITY_JSON_UTF8)],Json(json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{library}/followers"),"type":"Collection","totalItems":count}))).into_response())
}

async fn named_library(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    if !super::ap_requested(&headers) && q.page.is_none() {
        let uri = crate::music_library_federation::uri(&state.config.domain, id);
        crate::music_library_federation::owner(&state, &uri)
            .await?
            .ok_or(ApiError::NotFound)?;
        return Ok(
            axum::response::Redirect::temporary(&format!("/music/collections/{id}"))
                .into_response(),
        );
    }
    super::require_ap_accept(&headers)?;
    Ok((
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(crate::music_library_federation::document(&state, id, q.page).await?),
    )
        .into_response())
}
async fn named_followers(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let uri = crate::music_library_federation::uri(&state.config.domain, id);
    crate::music_library_federation::owner(&state, &uri)
        .await?
        .ok_or(ApiError::NotFound)?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM music_follows f JOIN accounts a ON a.id=f.follower_id WHERE f.target_uri=$1 AND f.local_target AND NOT f.pending AND a.suspended_at IS NULL AND a.deleted_at IS NULL")
        .bind(&uri).fetch_one(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    Ok(([(header::CONTENT_TYPE,plamenu_ap::ACTIVITY_JSON_UTF8),(header::CACHE_CONTROL,"no-store")],Json(json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{uri}/followers"),"type":"Collection","totalItems":count}))).into_response())
}

async fn entity_response(
    state: &AppState,
    kind: crate::music_entities::Kind,
    id: i64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    if !super::ap_requested(headers) {
        return Ok(
            axum::response::Redirect::temporary(&format!("/music/{}/{id}", kind.path()))
                .into_response(),
        );
    }
    super::require_ap_accept(headers)?;
    Ok((
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(crate::music_entities::document(state, kind, id).await?),
    )
        .into_response())
}
async fn artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    entity_response(&state, crate::music_entities::Kind::Artist, id, &headers).await
}
async fn album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    entity_response(&state, crate::music_entities::Kind::Album, id, &headers).await
}
async fn entity_followers(
    state: &AppState,
    kind: crate::music_entities::Kind,
    id: i64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(headers)?;
    let target = crate::music_entities::uri(&state.config.domain, kind, id);
    crate::music_entities::resource_owner(state, &target)
        .await?
        .ok_or(ApiError::NotFound)?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM music_follows f JOIN accounts a ON a.id=f.follower_id WHERE f.target_uri=$1 AND f.local_target AND NOT f.pending AND a.suspended_at IS NULL AND a.deleted_at IS NULL").bind(&target).fetch_one(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    Ok(([(header::CONTENT_TYPE,plamenu_ap::ACTIVITY_JSON_UTF8),(header::CACHE_CONTROL,"no-store")],Json(json!({"@context":"https://www.w3.org/ns/activitystreams","id":format!("{target}/followers"),"type":"Collection","totalItems":count}))).into_response())
}
async fn artist_followers(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    entity_followers(&state, crate::music_entities::Kind::Artist, id, &headers).await
}
async fn album_followers(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    entity_followers(&state, crate::music_entities::Kind::Album, id, &headers).await
}
