use crate::{AppState, auth::CurrentUser, error::ApiError, notification_mail};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use plamenu_db::notification_delivery;
use serde_json::json;
fn response(items: &[notification_delivery::Preference]) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(json!({"items":items})),
    )
        .into_response()
}
pub async fn index(State(state): State<AppState>, user: CurrentUser) -> Result<Response, ApiError> {
    user.require_scope("read:notifications")?;
    Ok(response(
        &notification_delivery::preferences(&state.pool, user.account.id).await?,
    ))
}
pub async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.require_scope("write:notifications")?;
    let input: notification_mail::Preferences = super::params::parse_body(&headers, &body)?;
    notification_mail::validate(&input.items)?;
    notification_delivery::update(&state.pool, user.account.id, &input.items).await?;
    Ok(response(
        &notification_delivery::preferences(&state.pool, user.account.id).await?,
    ))
}
