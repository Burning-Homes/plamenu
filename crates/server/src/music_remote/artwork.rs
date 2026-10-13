//! Artwork is fetched through the server and decoded before any bytes are served.
use super::stream::Access;
use crate::{AppState, error::ApiError, media_processing};
use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use std::time::Duration;

const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub async fn serve(state: &AppState, id: i64, access: Access) -> Result<Response, ApiError> {
    tokio::time::timeout(Duration::from_secs(30), serve_inner(state, id, access))
        .await
        .map_err(|_| ApiError::ServiceUnavailable("Artwork fetch timed out".into()))?
}
async fn serve_inner(state: &AppState, id: i64, access: Access) -> Result<Response, ApiError> {
    access.authorize(state, id).await?;
    let resource = super::get(state, id, access.viewer()).await?;
    let url = resource
        .details
        .image_url
        .as_deref()
        .ok_or(ApiError::NotFound)?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, url).await? {
        return Err(ApiError::NotFound);
    }
    let fetched = state
        .federation
        .fetch_media_limited(url, MAX_BYTES)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if !crate::instance_policy::can_federate_url(
        &state.pool,
        &state.config.domain,
        &fetched.final_url,
    )
    .await?
    {
        return Err(ApiError::NotFound);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let mut params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    params.max_image_bytes = params.max_image_bytes.min(crate::music_artworks::MAX_BYTES);
    let image =
        media_processing::process_image_blocking(fetched.bytes, 1024.min(params.max_edge), params)
            .await?;
    access.authorize(state, id).await?;
    let current = super::get(state, id, access.viewer()).await?;
    if current.details.image_url.as_deref() != Some(url)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, url).await?
        || !crate::instance_policy::can_federate_url(
            &state.pool,
            &state.config.domain,
            &fetched.final_url,
        )
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok((
        [
            (header::CONTENT_TYPE, image.content_type),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        image.bytes,
    )
        .into_response())
}
