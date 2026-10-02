use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    video_playlists as service,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use plamenu_db::video_playlists as db;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/video_playlists", get(list).post(create))
        .route("/api/v1/video_playlists/import", post(import))
        .route("/api/v1/video_playlists/{id}", get(show).post(change))
}
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/ap/video-playlists/{id}", get(ap_root))
        .route("/ap/video-playlists/{id}/pages/{page}", get(ap_page))
        .route("/ap/video-playlists/{id}/entries/{entry}", get(ap_entry))
        .route("/ap/accounts/{id}/playlists", get(ap_account))
        .route("/users/{username}/playlists", get(ap_legacy_account))
}

pub fn summary(state: &AppState, p: &db::Playlist) -> Value {
    json!({"id":p.id.to_string(),"uri":service::uri(state,p),"url":format!("https://{}/videos/playlists/{}",state.config.domain,p.id),"uuid":p.uuid,"creator_id":p.creator_id.to_string(),"channel_id":p.channel_id.to_string(),"name":p.name,"description":p.description,"visibility":p.visibility,"revision":p.revision,"remote":p.remote_uri.is_some()})
}
pub async fn entity(
    state: &AppState,
    p: &db::Playlist,
    viewer: Option<i64>,
) -> Result<Value, ApiError> {
    let mut value = summary(state, p);
    let entries = db::entries(&state.pool, p.id).await?;
    let ids = entries
        .iter()
        .filter_map(|e| e.status_id)
        .collect::<Vec<_>>();
    let live_ids:Vec<i64>=sqlx::query_scalar("SELECT id FROM statuses WHERE id=ANY($1) AND deleted_at IS NULL AND reblog_of_id IS NULL AND object_type='Video'\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos").bind(&ids).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let stored = plamenu_db::status::find_by_ids(&state.pool, &live_ids).await?;
    let stored = crate::entities::filter_viewable(&state.pool, &stored, viewer)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect::<std::collections::HashMap<_, _>>();
    let mut items = Vec::with_capacity(entries.len());
    for e in &entries {
        let video = e.status_id.and_then(|id| stored.get(&id));
        let available = if let Some(video) = video {
            p.visibility == "private" || matches!(video.visibility.as_str(), "public" | "unlisted")
        } else {
            false
        };
        items.push(json!({"id":e.id.to_string(),"position":e.position,"status_id":if available{e.status_id.map(|id|id.to_string())}else{None},"video_uri":if (e.status_id.is_none()&&!e.video_uri.is_empty())||available{Some(&e.video_uri)}else{None},"name":if available{video.and_then(|s|s.title.as_deref())}else{None},"available":available,"unresolved":e.status_id.is_none()&&!e.video_uri.is_empty(),"start_seconds":e.start_seconds,"stop_seconds":e.stop_seconds,"url":format!("/videos/playlists/{}?entry={}",p.id,e.id)}));
    }
    value["entries"] = json!(items);
    value["can_edit"] = json!(if let Some(viewer) = viewer {
        service::can_edit(state, p, viewer).await?
    } else {
        false
    });
    Ok(value)
}
#[derive(Default, Deserialize)]
struct ListQuery {
    mine: Option<bool>,
    max_id: Option<i64>,
}
async fn list(
    State(state): State<AppState>,
    MaybeUser(user): MaybeUser,
    Query(q): Query<ListQuery>,
) -> Result<Response, ApiError> {
    if let Some(user) = &user {
        user.require_scope("read:statuses")?;
    }
    let rows = service::list(
        &state,
        user.map(|u| u.account.id),
        q.mine.unwrap_or(false),
        q.max_id,
    )
    .await?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(json!(
            rows.iter().map(|p| summary(&state, p)).collect::<Vec<_>>()
        )),
    )
        .into_response())
}
async fn show(
    State(state): State<AppState>,
    MaybeUser(user): MaybeUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    if let Some(user) = &user {
        user.require_scope("read:statuses")?;
    }
    let viewer = user.map(|u| u.account.id);
    let p = service::readable(&state, id, viewer).await?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(entity(&state, &p, viewer).await?),
    )
        .into_response())
}
async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input = super::params::parse_body(&headers, &body)?;
    let p = service::create(&state, &current.account, &input).await?;
    Ok(Json(entity(&state, &p, Some(current.account.id)).await?))
}
async fn change(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input = super::params::parse_body(&headers, &body)?;
    let p = service::change(&state, &current.account, id, &input).await?;
    Ok(Json(summary(&state, &p)))
}
#[derive(Deserialize)]
struct Import {
    url: String,
}
async fn import(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:statuses")?;
    let input: Import = super::params::parse_body(&headers, &body)?;
    let p = crate::video_playlists_remote::import(&state, input.url.trim(), None).await?;
    Ok(Json(entity(&state, &p, Some(current.account.id)).await?))
}
async fn ap_playlist(state: &AppState, id: i64) -> Result<db::Playlist, ApiError> {
    let p = service::readable(state, id, None).await?;
    if p.remote_uri.is_some() {
        return Err(ApiError::NotFound);
    }
    Ok(p)
}
fn ap_response(value: Value) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/activity+json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Json(value),
    )
        .into_response()
}
async fn ap_root(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let p = ap_playlist(&state, id).await?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    Ok(ap_response(
        service::object(&state, &mut conn, &p, None).await?,
    ))
}
async fn ap_page(
    State(state): State<AppState>,
    Path((id, page)): Path<(i64, usize)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let p = ap_playlist(&state, id).await?;
    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(plamenu_db::DbError::from)?;
    Ok(ap_response(
        service::object(&state, &mut conn, &p, Some(page)).await?,
    ))
}
async fn ap_entry(
    State(state): State<AppState>,
    Path((id, entry)): Path<(i64, i64)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(&headers)?;
    let p = ap_playlist(&state, id).await?;
    let e = db::entries(&state.pool, id)
        .await?
        .into_iter()
        .find(|e| e.id == entry)
        .ok_or(ApiError::NotFound)?;
    let mut value = service::element(&state, &p, &e);
    if let Some(status) = e.status_id {
        let video = plamenu_db::status::find_by_id(&state.pool, status).await?;
        let visible = if let Some(s) = video {
            !plamenu_db::status::is_deleted(&state.pool, s.id).await?
                && crate::entities::can_view(&state.pool, &s, None).await?
        } else {
            false
        };
        if !visible {
            value["url"] = Value::Null;
        }
    }
    Ok(ap_response(value))
}
async fn account_collection(
    state: &AppState,
    account: i64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    super::require_ap_accept(headers)?;
    let actor = plamenu_db::account::find_by_id(&state.pool, account)
        .await?
        .filter(|a| a.is_local() && !a.suspended())
        .ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::account_id_visible(&state.pool, actor.id).await? {
        return Err(ApiError::NotFound);
    }
    // 50 channels × 100 active playlists is the enforced native-account cap.
    let rows:Vec<db::Playlist>=sqlx::query_as("SELECT p.* FROM video_playlists p JOIN video_channels c ON c.account_id=p.channel_id JOIN accounts a ON a.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id WHERE p.deleted_at IS NULL AND p.remote_uri IS NULL AND p.visibility='public' AND a.suspended_at IS NULL AND o.suspended_at IS NULL AND (c.owner_account_id=$1 OR c.account_id=$1) ORDER BY p.id DESC LIMIT 5000").bind(account).fetch_all(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let url = format!(
        "{}/playlists",
        crate::entities::account_uri(&state.config.domain, &actor)
    );
    Ok(ap_response(
        json!({"@context":plamenu_ap::activity::quote_context(),"id":url,"type":"OrderedCollection","totalItems":rows.len(),"orderedItems":rows.iter().map(|p|service::uri(state,p)).collect::<Vec<_>>()}),
    ))
}
async fn ap_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    account_collection(&state, id, &headers).await
}
async fn ap_legacy_account(
    State(state): State<AppState>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let a = plamenu_db::account::find_public_local_by_username(&state.pool, &username)
        .await?
        .ok_or(ApiError::NotFound)?;
    account_collection(&state, a.id, &headers).await
}
