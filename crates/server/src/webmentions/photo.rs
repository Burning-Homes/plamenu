//! Only bounded decoded JPEG/PNG leave this proxy; a source URL never redirects.
use super::Mention;
use crate::{AppState, error::ApiError, media_processing};
use plamenu_db::DbError;
pub(crate) async fn cache(state: &AppState, url: Option<&str>) -> Option<(Vec<u8>, String)> {
    let url = url?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, url)
        .await
        .ok()?
    {
        return None;
    }
    let bytes = state.federation.fetch_feed_image(url).await.ok()?;
    if bytes.len() > 2 * 1024 * 1024 {
        return None;
    }
    let settings = state.settings_cache.get(&state.pool).await.ok()?;
    let mut params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    params.max_image_bytes = params.max_image_bytes.min(2 * 1024 * 1024);
    let image = media_processing::process_image_blocking(bytes, 128, params)
        .await
        .ok()?;
    (image.bytes.len() <= 131_072).then(|| (image.bytes, image.content_type.into()))
}
pub async fn get(state: &AppState, id: i64) -> Result<(Vec<u8>, String), ApiError> {
    let row: Mention = sqlx::query_as("SELECT * FROM webmentions WHERE id=$1 AND active")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(DbError::from)?
        .ok_or(ApiError::NotFound)?;
    if !super::visible(state, &row).await? {
        return Err(ApiError::NotFound);
    }
    let url = row
        .entry
        .as_ref()
        .and_then(|e| e.photo.as_deref())
        .ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, url).await? {
        return Err(ApiError::NotFound);
    }
    Ok((
        row.photo_bytes.ok_or(ApiError::NotFound)?,
        row.photo_type.ok_or(ApiError::NotFound)?,
    ))
}
