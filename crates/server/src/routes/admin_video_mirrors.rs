//! Explicit operator approvals and bounded native mirror jobs.
use crate::{AppState, auth::AdminUser, error::ApiError, video_mirrors};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use plamenu_db::{role::permission, video_mirrors as db};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/admin/video_mirrors", get(index).post(create))
        .route("/api/v1/admin/video_mirrors/{id}", get(show).delete(remove))
        .route(
            "/api/v1/admin/video_mirror_approvals/{domain}",
            put(approval),
        )
}
fn private(value: Value) -> Response {
    ([(header::CACHE_CONTROL, "private, no-store")], Json(value)).into_response()
}
fn view(item: &db::Mirror) -> Value {
    json!({"id":item.id.to_string(),"media_id":item.media_id.to_string(),
        "status_id":item.status_id.to_string(),"state":item.state,"generation":item.generation,
        "reserved_bytes":item.reserved_bytes,"expires_at":item.expires_at.unix_timestamp(),
        "last_error":item.last_error})
}
async fn index(State(state): State<AppState>, admin: AdminUser) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_FEDERATION, false)?;
    let items =
        sqlx::query_as::<_, db::Mirror>("SELECT * FROM video_mirrors ORDER BY id DESC LIMIT 100")
            .fetch_all(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?;
    Ok(private(
        json!({"charged_bytes":db::charged(&state.pool).await?,"limit":100,
        "items":items.iter().map(view).collect::<Vec<_>>()}),
    ))
}
async fn show(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_FEDERATION, false)?;
    Ok(private(view(
        &db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?,
    )))
}
#[derive(Deserialize)]
struct Request {
    status_id: i64,
    #[serde(default)]
    consent_attestation: String,
}
async fn create(
    State(state): State<AppState>,
    admin: AdminUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_FEDERATION, true)?;
    let input: Request = super::params::parse_body(&headers, &body)?;
    let item = video_mirrors::request(
        &state,
        admin.current.account.id,
        input.status_id,
        &input.consent_attestation,
    )
    .await?;
    Ok(private(view(&item)))
}
#[derive(Deserialize)]
struct Approval {
    allow_source: bool,
    allow_mirror: bool,
}
async fn approval(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(domain): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_FEDERATION, true)?;
    let input: Approval = super::params::parse_body(&headers, &body)?;
    video_mirrors::approve(
        &state,
        admin.current.account.id,
        &domain,
        input.allow_source,
        input.allow_mirror,
    )
    .await?;
    Ok(private(json!({"updated":true})))
}
async fn remove(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_FEDERATION, true)?;
    video_mirrors::withdraw(&state, id).await?;
    Ok(private(json!({"withdrawn":true})))
}
