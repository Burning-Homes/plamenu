//! Reuse owned uploads as independent post attachments. Copies have their own
//! storage keys so deleting either publication cannot break the other one.
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, id, media};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

pub async fn choices(state: &AppState, owner: i64) -> Result<Vec<Value>, ApiError> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT m.id, COALESCE(NULLIF(s.title,''),NULLIF(m.description,''),NULLIF(s.text,''),m.content_type)
         FROM media_attachments m LEFT JOIN statuses s ON s.id=m.status_id
         WHERE m.account_id=$1 AND m.file_name IS NOT NULL AND m.processing='complete'
           AND m.live_state IS NULL AND (s.id IS NULL OR s.deleted_at IS NULL)
           AND NOT EXISTS(SELECT 1 FROM video_drafts d WHERE d.media_id=m.id AND d.state<>'ready')
         ORDER BY m.id DESC LIMIT 30 -- STUBFILTER: only the owner's live publications")
        .bind(owner).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let mut choices = Vec::new();
    let media = media::find_by_ids(
        &state.pool,
        &rows.iter().map(|row| row.0).collect::<Vec<_>>(),
    )
    .await?;
    for (id, name) in rows {
        let item = media
            .iter()
            .find(|item| item.id == id)
            .map(|item| crate::entities::media_json(&state.config.domain, item, false));
        choices.push(json!({"key":format!("media:{id}"),"name":name.chars().take(100).collect::<String>(),"media":item}));
    }
    for track in crate::music_assets::catalogue(state, Some(owner), true, "", None)
        .await?
        .into_iter()
        .take(30)
    {
        choices.push(json!({"key":format!("track:{}",track.id),"name":format!("{} — {}",track.title,track.artist),"media":{"type":"audio","url":format!("/music/assets/{}/stream",track.id),"description":track.title}}));
    }
    Ok(choices)
}

async fn copy_file(state: &AppState, source: &str, target: &str) -> Result<i64, ApiError> {
    let io = |error| ApiError::Internal(Box::new(error));
    let mut opened = state.media.open(source).await.map_err(io)?;
    let temporary = tempfile::NamedTempFile::new().map_err(io)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen().map_err(io)?);
    let copied = tokio::io::copy(&mut opened.reader, &mut output)
        .await
        .map_err(io)?;
    output.flush().await.map_err(io)?;
    state
        .media
        .put_file(target, temporary.path())
        .await
        .map_err(io)?;
    Ok(i64::try_from(copied).unwrap_or(i64::MAX))
}

async fn owned_track(
    state: &AppState,
    owner: i64,
    id: i64,
) -> Result<crate::music_assets::Asset, ApiError> {
    let item = crate::music_assets::get(state, id, Some(owner)).await?;
    if item.owner_id != owner {
        return Err(ApiError::NotFound);
    }
    crate::music_local::playback::prepare_publication(state, owner, id).await?;
    crate::music_assets::get(state, id, Some(owner)).await
}

async fn owned_upload(state: &AppState, owner: i64, id: i64) -> Result<media::Media, ApiError> {
    let item = media::find_owned(&state.pool, id, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let unfinished: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM video_drafts WHERE media_id=$1 AND state<>'ready')",
    )
    .bind(id)
    .fetch_one(&state.pool)
    .await
    .map_err(DbError::from)?;
    if item.processing != "complete"
        || item.file_name.is_none()
        || item.live_state.is_some()
        || unfinished
    {
        return Err(ApiError::Unprocessable(
            "This upload is not ready to attach".into(),
        ));
    }
    Ok(item)
}

pub async fn reuse(state: &AppState, owner: i64, key: &str) -> Result<i64, ApiError> {
    let (kind, source_id) = key.split_once(':').ok_or(ApiError::NotFound)?;
    let source_id = source_id.parse::<i64>().map_err(|_| ApiError::NotFound)?;
    let source = match kind {
        "media" => Some(owned_upload(state, owner, source_id).await?),
        "track" => None,
        _ => return Err(ApiError::NotFound),
    };
    let asset = if kind == "track" {
        Some(owned_track(state, owner, source_id).await?)
    } else {
        None
    };
    let original = source
        .as_ref()
        .and_then(|m| m.file_name.as_deref())
        .or_else(|| asset.as_ref().map(|a| a.stream_file.as_str()))
        .ok_or(ApiError::NotFound)?;
    let media_id = id::next();
    let stem = crate::media_processing::storage_stem(media_id);
    let file_name = format!("{stem}.{}", original.rsplit('.').next().unwrap_or("bin"));
    let small_file = source.as_ref().and_then(|m| m.small_file_name.as_deref());
    let small_name =
        small_file.map(|name| format!("{stem}.small.{}", name.rsplit('.').next().unwrap_or("jpg")));
    let result = async {
        let bytes = copy_file(state, original, &file_name).await?;
        let small_bytes = if let Some((from, to)) = small_file.zip(small_name.as_deref()) {
            Some(copy_file(state, from, to).await?)
        } else {
            None
        };
        let content_type = source.as_ref().map_or_else(
            || crate::media_processing::content_type_for(original),
            |m| m.content_type.as_str(),
        );
        let mut input = media::NewLocalMedia::new(owner, media_id, &file_name, content_type);
        if let Some(source) = &source {
            input.kind = source.kind_or_derived();
            input.description = source.description.as_deref();
            input.width = source.width;
            input.height = source.height;
            input.duration = source.duration;
            input.bitrate = source.bitrate;
            input.frame_rate = source.frame_rate.as_deref();
            input.blurhash = source.blurhash.as_deref();
            if let (Some(name), Some(width), Some(height)) = (
                small_name.as_deref(),
                source.small_width,
                source.small_height,
            ) {
                input.small = Some(media::SmallStyle {
                    file_name: name,
                    width,
                    height,
                });
            }
        } else if let Some(asset) = &asset {
            input.kind = "audio";
            input.duration = Some(asset.duration);
            input.description = Some(&asset.title);
        }
        media::create_local(&state.pool, input).await?;
        media::set_file_sizes(&state.pool, media_id, bytes, small_bytes).await?;
        if let Some(source) = &source {
            media::update_accessibility(
                &state.pool,
                media_id,
                owner,
                media::AccessibilityUpdate {
                    transcript: source.transcript.as_deref(),
                    caption_vtt: Some(source.caption_vtt.as_deref()),
                    decorative: source.decorative,
                    audio_described: source.audio_described,
                    visuals_conveyed_in_audio: source.visuals_conveyed_in_audio,
                },
            )
            .await?;
        }
        Ok::<_, ApiError>(media_id)
    }
    .await;
    if result.is_err() {
        let _ = media::delete_unattached(&state.pool, media_id, owner).await;
        let _ = state.media.delete(&file_name).await;
        if let Some(name) = small_name {
            let _ = state.media.delete(&name).await;
        }
    }
    result
}
