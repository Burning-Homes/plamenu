use crate::{AppState, auth::AdminUser, error::ApiError, music_jobs};
use axum::{
    Json,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use plamenu_db::role::permission;
pub async fn index(
    State(state): State<AppState>,
    admin: AdminUser,
    Query(q): Query<music_jobs::Query>,
) -> Result<Response, ApiError> {
    admin.require(permission::VIEW_DEVOPS, false)?;
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(music_jobs::page(&state, &q).await?),
    )
        .into_response())
}
pub async fn cancel(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(r): Json<music_jobs::Cancellation>,
) -> Result<Response, ApiError> {
    admin.require(permission::MANAGE_SETTINGS, true)?;
    match r {
        music_jobs::Cancellation::One(r)=>{music_jobs::cancel(&state,admin.current.account.id,&r).await?;Ok(axum::http::StatusCode::NO_CONTENT.into_response())},
        music_jobs::Cancellation::Many(batch)=>Ok(Json(serde_json::json!({"cancelled":music_jobs::cancel_many(&state,admin.current.account.id,&batch.jobs).await?})).into_response()),
    }
}
