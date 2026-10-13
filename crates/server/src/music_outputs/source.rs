//! Decoders receive owned local paths or bytes fetched through federation guards.
use crate::{AppState, error::ApiError, music_collections::Entry};
use plamenu_db::{DbError, media};
use std::path::PathBuf;
#[derive(PartialEq, Eq)]
pub enum Input {
    File(crate::music_original::ProcessInput),
    Retained(PathBuf),
    Origin(String),
}
pub async fn resolve(state: &AppState, entry: &Entry, owner: i64) -> Result<Input, ApiError> {
    let entry = crate::music_remote::readable_entries(
        &state.pool,
        std::slice::from_ref(entry),
        Some(owner),
    )
    .await?
    .into_iter()
    .next()
    .ok_or(ApiError::NotFound)?;
    let entry = &entry;
    // This is the same current source decision as the native player, including
    // withdrawn publications, actor retirement and private membership grants.
    if crate::music_collections::sources(state, std::slice::from_ref(entry), Some(owner))
        .await?
        .is_empty()
    {
        return Err(ApiError::NotFound);
    }
    if let Some(asset) = entry.asset_id {
        let row = crate::music_assets::get(state, asset, Some(owner)).await?;
        return Ok(Input::File(
            crate::music_original::process_input(state, &row).await?,
        ));
    }
    let url = if let Some(remote) = entry.remote_id {
        crate::music_remote::stream::source(state, remote, owner)
            .await?
            .url
    } else if let Some(episode) = entry.episode_id {
        sqlx::query_scalar::<_, String>("SELECT audio_url FROM music_podcast_episodes WHERE id=$1")
            .bind(episode)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?
            .ok_or(ApiError::NotFound)?
    } else if let Some(status) = entry.status_id {
        let mut rows = media::for_statuses(&state.pool, &[status]).await?;
        let attachment = rows
            .remove(&status)
            .and_then(|rows| {
                rows.into_iter()
                    .find(|item| item.content_type.starts_with("audio/"))
            })
            .ok_or(ApiError::NotFound)?;
        if let Some(file) = attachment
            .file_name
            .and_then(|key| state.media.local_path(&key))
        {
            return Ok(Input::Retained(file));
        }
        attachment.remote_url.ok_or(ApiError::NotFound)?
    } else {
        return Err(ApiError::NotFound);
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &url).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Input::Origin(url))
}

pub async fn copy_origin(
    state: &AppState,
    url: &str,
    mut input: tokio::process::ChildStdin,
) -> Result<(), ApiError> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let mut source = state
        .federation
        .fetch_enclosure(url, None)
        .await
        .map_err(|_| ApiError::BadGateway("Could not read HTTP audio source".into()))?;
    if source.status != 200
        || source
            .content_length
            .is_some_and(|length| length > 16 * 1024 * 1024 * 1024)
    {
        return Err(ApiError::BadGateway(
            "Invalid HTTP audio source response".into(),
        ));
    }
    let mut received = 0_u64;
    while let Some(chunk) =
        tokio::time::timeout(std::time::Duration::from_secs(30), source.bytes.next())
            .await
            .map_err(|_| ApiError::BadGateway("HTTP audio source stalled".into()))?
    {
        let bytes = chunk.map_err(|_| ApiError::BadGateway("HTTP audio source failed".into()))?;
        received = received.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if received > 16 * 1024 * 1024 * 1024
            || source
                .content_length
                .is_some_and(|length| received > length)
        {
            return Err(ApiError::BadGateway(
                "HTTP audio source exceeded its byte limit".into(),
            ));
        }
        input
            .write_all(&bytes)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
    }
    if source
        .content_length
        .is_some_and(|length| received != length)
    {
        return Err(ApiError::BadGateway("HTTP audio source ended early".into()));
    }
    Ok(())
}
