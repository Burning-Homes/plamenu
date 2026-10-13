//! Mounted folders are assigned by operators, then managed by their owners.
use crate::music_local::files;
use crate::{
    AppState,
    auth::{AdminUser, CurrentUser},
    error::ApiError,
    music_local::folders,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use plamenu_db::role::permission;
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
        .route("/api/v1/music/folders", get(index))
        .route(
            "/api/v1/music/folders/{id}",
            get(show).put(refresh).delete(disconnect),
        )
        .route("/api/v1/music/folders/{id}/files", get(file_list))
        .route(
            "/api/v1/music/folders/{folder}/files/{file}",
            axum::routing::post(file_mutate),
        )
        .route(
            "/api/v1/music/folders/{folder}/files/{file}/restore",
            axum::routing::post(restore),
        )
        .route("/api/v1/admin/music/roots", get(roots))
        .route("/api/v1/admin/music/folders", axum::routing::post(assign))
}
async fn index(State(state): State<AppState>, user: CurrentUser) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    Ok(response(folders::list(&state, user.account.id).await?))
}
async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(folder): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    Ok(response(
        folders::get(&state, user.account.id, folder).await?,
    ))
}
#[derive(Deserialize)]
struct Refresh {
    revision: i64,
    watch: bool,
    enabled: bool,
    #[serde(default)]
    options: Option<crate::music_local::options::Options>,
}
async fn refresh(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(folder): Path<i64>,
    Json(input): Json<Refresh>,
) -> Result<Response, ApiError> {
    user.require_scope("write:media")?;
    Ok(response(
        folders::configure(
            &state,
            user.account.id,
            folder,
            input.revision,
            input.watch,
            input.enabled,
            input.options.as_ref(),
        )
        .await?,
    ))
}
async fn roots(State(state): State<AppState>, admin: AdminUser) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_SETTINGS, false)?;
    Ok(response(state.config.music_local_roots.iter().map(|root|serde_json::json!({"id":root.id,"label":root.label,"allow_tag_writeback":root.allow_tag_writeback,"allow_rename":root.allow_rename,"allow_delete":root.allow_delete})).collect::<Vec<_>>()))
}
async fn assign(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(input): Json<folders::New>,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_SETTINGS, true)?;
    admin.require(permission::MANAGE_USERS, true)?;
    Ok(response(folders::assign(&state, &input).await?))
}

#[derive(Default, Deserialize)]
struct FilePage {
    before: Option<i64>,
}
async fn file_list(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(folder): Path<i64>,
    axum::extract::Query(page): axum::extract::Query<FilePage>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    Ok(response(
        files::list(&state, user.account.id, folder, page.before).await?,
    ))
}
async fn file_mutate(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((folder, file)): Path<(i64, i64)>,
    Json(input): Json<files::Mutation>,
) -> Result<Response, ApiError> {
    user.require_scope("write:media")?;
    files::mutate(&state, user.account.id, folder, file, &input).await?;
    Ok(response(serde_json::json!({})))
}

#[derive(Deserialize)]
struct Revision {
    revision: i64,
}
async fn disconnect(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(folder): Path<i64>,
    Json(input): Json<Revision>,
) -> Result<Response, ApiError> {
    user.require_scope("write:media")?;
    folders::disconnect(&state, user.account.id, folder, input.revision).await?;
    Ok(response(serde_json::json!({})))
}
async fn restore(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((folder, file)): Path<(i64, i64)>,
    Json(input): Json<Revision>,
) -> Result<Response, ApiError> {
    user.require_scope("write:media")?;
    files::restore(&state, user.account.id, folder, file, input.revision).await?;
    Ok(response(serde_json::json!({})))
}
