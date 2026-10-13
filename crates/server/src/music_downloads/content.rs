//! Item bytes are spooled with finite bounds before joining the ZIP.
use super::{Download, selection::Item};
use crate::{AppState, error::ApiError, music_collections::Entry};
use plamenu_db::{DbError, media};
use std::path::Path;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
enum Location {
    Stored(String, String),
    Original(Box<crate::music_assets::Asset>),
    Origin(String),
}
async fn location(state: &AppState, entry: &Entry, owner: i64) -> Result<Location, ApiError> {
    if crate::music_collections::sources(state, std::slice::from_ref(entry), Some(owner))
        .await?
        .is_empty()
    {
        return Err(ApiError::NotFound);
    }
    if let Some(asset) = entry.asset_id {
        let row = crate::music_assets::get(state, asset, Some(owner)).await?;
        return Ok(Location::Original(Box::new(row)));
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
        let row = rows
            .remove(&status)
            .and_then(|rows| {
                rows.into_iter()
                    .find(|row| row.content_type.starts_with("audio/"))
            })
            .ok_or(ApiError::NotFound)?;
        if let Some(key) = row.file_name {
            return Ok(Location::Stored(key, extension(&row.content_type).into()));
        }
        row.remote_url.ok_or(ApiError::NotFound)?
    } else {
        return Err(ApiError::NotFound);
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &url).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Location::Origin(url))
}
pub async fn proof(state: &AppState, entry: &Entry, owner: i64) -> Result<String, ApiError> {
    let location = location(state, entry, owner).await?;
    if let Some(asset) = entry.asset_id {
        let row = crate::music_assets::get(state, asset, Some(owner)).await?;
        let original = crate::music_original::proof(state, &row).await?;
        return Ok(format!("{}:{}:{}", original, row.audio_hash, row.revision));
    }
    Ok(match location {
        Location::Stored(key, _) => key,
        Location::Original(_) => unreachable!("asset proof handled above"),
        Location::Origin(url) => url,
    })
}
pub async fn estimate(state: &AppState, owner: i64, items: &[Item]) -> Result<u64, ApiError> {
    let maximum = state.config.music.max_download_mib * 1024 * 1024;
    let item_max = state.config.music.max_download_item_mib * 1024 * 1024;
    let mut bytes = 0_u64;
    let mut largest = 0_u64;
    for item in items {
        let size = if let Some(asset) = item.entry.asset_id {
            let row = crate::music_assets::get(state, asset, Some(owner)).await?;
            if row.indexed_original {
                crate::music_original::open_info(state, &row)
                    .await?
                    .reader
                    .len
            } else {
                u64::try_from(row.original_bytes).unwrap_or(item_max)
            }
        } else if let Some(episode) = item.entry.episode_id {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT audio_length FROM music_podcast_episodes WHERE id=$1",
            )
            .bind(episode)
            .fetch_optional(&state.pool)
            .await
            .map_err(DbError::from)?
            .flatten()
            .and_then(|size| u64::try_from(size).ok())
            .unwrap_or(item_max)
        } else {
            item_max
        };
        largest = largest.max(size.min(item_max));
        bytes = bytes.saturating_add(size.min(item_max)).min(maximum);
    }
    // Headers/central directory plus one complete item and the final store copy.
    Ok(bytes
        .saturating_add(u64::try_from(items.len() + 1).expect("bounded selection") * 4096)
        .saturating_mul(2)
        .saturating_add(largest))
}
fn extension(kind: &str) -> &'static str {
    match kind.split(';').next().unwrap_or_default() {
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/aac" => "aac",
        "audio/mp4" => "m4a",
        _ => "audio",
    }
}
pub struct Spooled {
    pub file: tempfile::NamedTempFile,
    pub extension: String,
    pub bytes: u64,
}
pub async fn spool(
    state: &AppState,
    job: &Download,
    item: &Item,
    root: &Path,
) -> Result<Spooled, ApiError> {
    for attempt in 0..3 {
        match spool_once(state, job, item, root).await {
            Ok(file) => return Ok(file),
            Err(ApiError::BadGateway(_)) if attempt < 2 => {
                super::worker::heartbeat(state, job).await?;
                tokio::select! {
                    ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Archive worker stopped".into())),
                    ()=tokio::time::sleep(std::time::Duration::from_secs(2<<attempt))=>{},
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(ApiError::BadGateway("Archive source failed".into()))
}
async fn spool_once(
    state: &AppState,
    job: &Download,
    item: &Item,
    root: &Path,
) -> Result<Spooled, ApiError> {
    super::selection::playable(state, job.owner_id, item).await?;
    let location = location(state, &item.entry, job.owner_id).await?;
    let file = tempfile::Builder::new()
        .prefix("member-")
        .tempfile_in(root)
        .map_err(super::internal)?;
    let mut output = tokio::fs::File::from_std(file.reopen().map_err(super::internal)?);
    let mut checked = std::time::Instant::now();
    let (bytes, extension) = match location {
        Location::Stored(_, _) | Location::Original(_) => {
            let (mut source, extension) = match location {
                Location::Original(asset) => {
                    let original = crate::music_original::open_info(state, &asset).await?;
                    (original.reader, original.format)
                }
                Location::Stored(key, extension) => (
                    state.media.open(&key).await.map_err(|error| {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            ApiError::NotFound
                        } else {
                            super::internal(error)
                        }
                    })?,
                    extension,
                ),
                Location::Origin(_) => unreachable!("origin handled separately"),
            };
            if source.len > state.config.music.max_download_item_mib * 1024 * 1024 {
                return Err(ApiError::Unprocessable(
                    "Archive item exceeds its size limit".into(),
                ));
            }
            let mut buffer = vec![0; 65536];
            let mut received = 0_u64;
            loop {
                let count = source
                    .reader
                    .read(&mut buffer)
                    .await
                    .map_err(super::internal)?;
                if count == 0 {
                    break;
                }
                received = received.saturating_add(u64::try_from(count).expect("bounded chunk"));
                write(
                    state,
                    job,
                    item,
                    &mut output,
                    &buffer[..count],
                    received,
                    &mut checked,
                )
                .await?;
            }
            if received != source.len {
                return Err(ApiError::BadGateway("Archive source ended early".into()));
            }
            (received, extension)
        }
        Location::Origin(url) => origin(state, job, item, &url, &mut output, &mut checked).await?,
    };
    if bytes == 0 {
        return Err(ApiError::BadGateway("Archive source is empty".into()));
    }
    output.flush().await.map_err(super::internal)?;
    super::selection::playable(state, job.owner_id, item).await?;
    Ok(Spooled {
        file,
        extension,
        bytes,
    })
}
async fn origin(
    state: &AppState,
    job: &Download,
    item: &Item,
    url: &str,
    output: &mut tokio::fs::File,
    checked: &mut std::time::Instant,
) -> Result<(u64, String), ApiError> {
    use futures_util::StreamExt as _;
    let mut source =
        state
            .federation
            .fetch_enclosure(url, None)
            .await
            .map_err(|error| match error {
                plamenu_federation::FederationError::Status(401 | 403 | 404 | 410) => {
                    ApiError::NotFound
                }
                _ => ApiError::BadGateway("Could not fetch archive audio".into()),
            })?;
    if source.status != 200
        || source
            .content_length
            .is_some_and(|length| length > state.config.music.max_download_item_mib * 1024 * 1024)
    {
        return Err(ApiError::BadGateway(
            "Invalid archive source response".into(),
        ));
    }
    let mut received = 0_u64;
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(30), source.bytes.next());
        tokio::pin!(chunk);
        let next = loop {
            tokio::select! {
                result=&mut chunk=>break result.map_err(|_|ApiError::BadGateway("Archive source stalled".into()))?,
                ()=tokio::time::sleep(std::time::Duration::from_secs(1))=>{
                    super::worker::heartbeat(state,job).await?;
                    super::selection::playable(state,job.owner_id,item).await?;
                },
                ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Archive worker stopped".into())),
            }
        };
        let Some(bytes) = next else {
            break;
        };
        let bytes = bytes.map_err(|_| ApiError::BadGateway("Archive source failed".into()))?;
        received = received.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if source
            .content_length
            .is_some_and(|length| received > length)
        {
            return Err(ApiError::BadGateway(
                "Archive source exceeded declared size".into(),
            ));
        }
        write(state, job, item, output, &bytes, received, checked).await?;
    }
    if source
        .content_length
        .is_some_and(|length| received != length)
    {
        return Err(ApiError::BadGateway("Archive source ended early".into()));
    }
    Ok((received, extension(&source.content_type).into()))
}
async fn write(
    state: &AppState,
    job: &Download,
    item: &Item,
    output: &mut tokio::fs::File,
    bytes: &[u8],
    received: u64,
    checked: &mut std::time::Instant,
) -> Result<(), ApiError> {
    if received > state.config.music.max_download_item_mib * 1024 * 1024 {
        return Err(ApiError::Unprocessable(
            "Archive item exceeds its size limit".into(),
        ));
    }
    if checked.elapsed() >= std::time::Duration::from_secs(1) {
        super::worker::heartbeat(state, job).await?;
        super::selection::playable(state, job.owner_id, item).await?;
        *checked = std::time::Instant::now();
    }
    if state.shutdown.is_cancelled() {
        return Err(ApiError::Conflict("Archive worker stopped".into()));
    }
    output.write_all(bytes).await.map_err(super::internal)
}
