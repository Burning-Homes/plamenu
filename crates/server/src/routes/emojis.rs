//! The `ActivityPub` representation of local custom emoji
//! (`GET /emojis/{id}`), like Mastodon's emoji endpoint — remote
//! servers dereference the `Emoji` tag entries our notes and actors carry.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;

use super::require_ap_accept;
use crate::AppState;
use crate::error::ApiError;

pub async fn get_emoji(
    State(state): State<AppState>,
    Path(emoji_id): Path<i64>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_ap_accept(&headers)?;
    let emoji = plamenu_db::custom_emoji::find_local_by_id(&state.pool, emoji_id)
        .await?
        .filter(|emoji| !emoji.disabled)
        .ok_or(ApiError::NotFound)?;
    let object = crate::emoji::ap_emoji_object(&state.config.domain, &emoji)?;
    Ok((
        [(header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8)],
        Json(object),
    ))
}
