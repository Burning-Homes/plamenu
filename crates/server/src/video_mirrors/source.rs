//! Canonical local mirror lookup for the normal JS and no-JS playback lanes.
//! Copy availability never overrides the original publication's authority.
use crate::{AppState, error::ApiError};
use futures_util::StreamExt;
use plamenu_db::video_mirrors as db;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

async fn media_for_url(state: &AppState, url: &str) -> Result<Option<i64>, ApiError> {
    if state.config.video_redundancy.is_none() {
        return Ok(None);
    }
    Ok(sqlx::query_scalar("SELECT m.media_id FROM video_mirrors m JOIN video_mirror_assets a ON a.mirror_id=m.id AND a.generation=m.generation WHERE m.state='ready' AND a.source_url=$1 ORDER BY m.id LIMIT 1")
        .bind(url).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?)
}

/// Metadata-only canonical fetch for the existing sparse no-JS gateway.
pub async fn fetch_metadata(
    state: &AppState,
    url: &str,
) -> Result<plamenu_federation::FetchedMedia, ApiError> {
    if let Some(media) = media_for_url(state, url).await?
        && let Some(bytes) = metadata(
            state,
            media,
            url,
            super::manifest::MAX_PLAYLIST_BYTES as u64,
        )
        .await?
    {
        return Ok(plamenu_federation::FetchedMedia {
            final_url: url.into(),
            content_type: "application/vnd.apple.mpegurl".into(),
            bytes,
        });
    }
    state
        .federation
        .fetch_media(url)
        .await
        .map_err(|_| ApiError::NotFound)
}

/// A mirror reader owns its file handle, not a whole-file allocation or a
/// detached copier. Dropping the compatibility response stops disk reads.
pub async fn fetch_range(
    state: &AppState,
    url: &str,
    start: u64,
    len: u64,
) -> Result<plamenu_federation::FetchedMediaRange, ApiError> {
    if let Some(media) = media_for_url(state, url).await?
        && let Some(stored) = asset(state, media, url).await?
        && let Ok(mut opened) = state.media.open(&stored.file_name).await
        && i64::try_from(opened.len).ok() == Some(stored.bytes)
        && start.checked_add(len).is_some_and(|end| end <= opened.len)
    {
        opened
            .reader
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        return Ok(plamenu_federation::FetchedMediaRange {
            final_url: url.into(),
            content_type: stored.content_type,
            total_len: opened.len,
            start,
            len,
            bytes: Box::pin(
                tokio_util::io::ReaderStream::with_capacity(opened.reader.take(len), 64 * 1024)
                    .map(|chunk| {
                        chunk
                            .map(|bytes| bytes.to_vec())
                            .map_err(plamenu_federation::FederationError::from)
                    }),
            ),
        });
    }
    state
        .federation
        .fetch_media_range(url, start, len)
        .await
        .map_err(|_| ApiError::NotFound)
}

async fn asset(state: &AppState, media: i64, url: &str) -> Result<Option<db::Asset>, ApiError> {
    if state.config.video_redundancy.is_none() {
        return Ok(None);
    }
    let item = sqlx::query_as::<_, db::Mirror>(
        "SELECT * FROM video_mirrors WHERE media_id=$1 AND state='ready' LIMIT 1",
    )
    .bind(media)
    .fetch_optional(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let Some(item) = item else {
        return Ok(None);
    };
    if !super::authorized(state, &item).await? {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, db::Asset>(
        "SELECT * FROM video_mirror_assets WHERE mirror_id=$1 AND generation=$2 AND source_url=$3",
    )
    .bind(item.id)
    .bind(item.generation)
    .bind(url)
    .fetch_optional(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?)
}

pub async fn available(state: &AppState, media: i64, url: &str) -> Result<bool, ApiError> {
    Ok(asset(state, media, url).await?.is_some())
}

pub async fn metadata(
    state: &AppState,
    media: i64,
    url: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>, ApiError> {
    let Some(stored) = asset(state, media, url).await? else {
        return Ok(None);
    };
    let Ok(opened) = state.media.open(&stored.file_name).await else {
        return Ok(None);
    };
    if opened.len > limit || i64::try_from(opened.len).ok() != Some(stored.bytes) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    opened
        .reader
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if bytes.len() as u64 > limit
        || super::worker::digest_hex(Sha256::digest(&bytes).as_slice()) != stored.sha256
    {
        return Ok(None);
    }
    // Recheck after I/O; withdrawal during a read must not warm another cache.
    if !available(state, media, url).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Some(bytes))
}

pub async fn range_to_file(
    state: &AppState,
    media: i64,
    url: &str,
    path: &std::path::Path,
    start: u64,
    len: u64,
) -> Result<Option<u64>, ApiError> {
    let Some(stored) = asset(state, media, url).await? else {
        return Ok(None);
    };
    let Ok(mut opened) = state.media.open(&stored.file_name).await else {
        return Ok(None);
    };
    if i64::try_from(opened.len).ok() != Some(stored.bytes)
        || start.checked_add(len).is_none_or(|end| end > opened.len)
    {
        return Ok(None);
    }
    opened
        .reader
        .seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut output = tokio::fs::File::create(path)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let written = tokio::io::copy(&mut opened.reader.take(len), &mut output)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    output
        .flush()
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if written != len || !available(state, media, url).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Some(written))
}
