//! Immutable, quota-accounted replacement posters for unpublished Video drafts.
//! Image encoding happens outside database locks. A reservation and journal
//! commit before any file write; the media row fences the final pointer swap.

use plamenu_db::{id, media_cleanup, video_draft, video_storage};

use crate::{AppState, error::ApiError, media_processing};

pub const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_RENDITION_BYTES: usize = 4 * 1024 * 1024;

struct Revision {
    media_id: i64,
    owner: i64,
    generation: i64,
    old_small: String,
    new_small: String,
    new_jpeg: String,
    canonical_jpeg: String,
}

pub async fn replace_draft_poster(
    state: &AppState,
    media_id: i64,
    owner: i64,
    input: Vec<u8>,
    alt: String,
) -> Result<(), ApiError> {
    let alt = alt.trim().to_owned();
    if !(1..=500).contains(&alt.chars().count()) {
        return Err(ApiError::Unprocessable(
            "Describe the custom poster in 1–500 characters".into(),
        ));
    }
    if input.is_empty() || input.len() > MAX_UPLOAD_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    if !media_processing::is_still_image(&input) || media_processing::is_animated_image(&input) {
        return Err(ApiError::Unprocessable(
            "Choose a still image for the poster".into(),
        ));
    }
    // Check ownership before spending CPU. Recheck under the media fence after
    // encoding and again before the final pointer swap.
    video_draft::preview(&state.pool, media_id, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let native =
        media_processing::process_native_video_poster_blocking(input.clone(), params.clone())
            .await?;
    let jpeg = media_processing::process_video_poster_blocking(input, params).await?;
    if native.extension != "avif"
        || jpeg.extension != "jpg"
        || native.bytes.is_empty()
        || jpeg.bytes.is_empty()
        || native.bytes.len() > MAX_RENDITION_BYTES
        || jpeg.bytes.len() > MAX_RENDITION_BYTES
    {
        return Err(ApiError::Unprocessable(
            "The poster could not be encoded within the video poster limit".into(),
        ));
    }
    let additional = u64::try_from(native.bytes.len() + jpeg.bytes.len()).unwrap_or(u64::MAX);
    let free = crate::storage::available_bytes(&state.config.media_dir)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let required = state
        .config
        .native_video
        .min_free_gib
        .saturating_mul(crate::config::NativeVideoConfig::GIB)
        .saturating_add(state.video_resources.usage().1)
        .saturating_add(additional);
    if free < required {
        return Err(ApiError::Unprocessable(
            "Not enough free space for a new video poster".into(),
        ));
    }
    let revision = reserve(state, media_id, owner, native.bytes.len(), jpeg.bytes.len()).await?;
    // A disconnected client must not cancel a storage write after reserving
    // durable quota. The task owns the entire short finalization window.
    let state = state.clone();
    tokio::spawn(async move { finalize(&state, revision, native, jpeg, alt).await })
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
}

async fn reserve(
    state: &AppState,
    media_id: i64,
    owner: i64,
    avif_size: usize,
    jpeg_size: usize,
) -> Result<Revision, ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    video_storage::lock(&mut tx).await?;
    let current = sqlx::query!(
        r#"SELECT m.file_name AS "file_name!", m.small_file_name AS "small_file_name!",
                  d.generation
           FROM video_drafts d JOIN media_attachments m ON m.id = d.media_id
           JOIN accounts a ON a.id = d.account_id JOIN accounts c ON c.id = d.channel_id
           WHERE d.media_id = $1 AND d.account_id = $2 AND m.account_id = $2
             AND d.state = 'ready' AND d.expires_at > clock_timestamp()
             AND m.status_id IS NULL AND m.processing = 'complete'
             AND m.file_name IS NOT NULL AND m.small_file_name IS NOT NULL
             AND EXISTS (SELECT 1 FROM video_draft_outputs o
                 WHERE o.file_name = m.small_file_name AND o.media_id = m.id
                   AND o.generation = d.generation AND NOT o.retired)
             AND a.suspended_at IS NULL AND c.suspended_at IS NULL
           FOR UPDATE OF m, d"#,
        media_id,
        owner,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?
    .ok_or(ApiError::Conflict(
        "The video draft is no longer ready for a poster change".into(),
    ))?;
    let stem = current
        .file_name
        .strip_suffix(".draft.mp4")
        .ok_or(ApiError::NotFound)?;
    let nonce = id::next();
    let new_small = format!("{stem}.draft.poster{nonce}.avif");
    let new_jpeg = format!("{stem}.draft.poster{nonce}.jpg");
    let keys = vec![new_small.clone(), new_jpeg.clone()];
    let sizes = vec![
        i64::try_from(avif_size).unwrap_or(i64::MAX),
        i64::try_from(jpeg_size).unwrap_or(i64::MAX),
    ];
    if !video_storage::reserve_locked(
        &mut tx,
        owner,
        media_id,
        &keys,
        &sizes,
        &state.config.native_video.storage_limits(),
    )
    .await?
    {
        return Err(ApiError::Unprocessable(
            "Video storage quota is full; the existing poster is unchanged".into(),
        ));
    }
    let cleanup_ids = vec![id::next(), id::next()];
    sqlx::query!(
        "INSERT INTO video_draft_outputs (file_name, media_id, generation, cleanup_job_id)
         SELECT entry.file_name, $2, $3, entry.cleanup_job_id
         FROM unnest($1::text[], $4::bigint[]) AS entry(file_name, cleanup_job_id)",
        &keys,
        media_id,
        current.generation,
        &cleanup_ids,
    )
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(Revision {
        media_id,
        owner,
        generation: current.generation,
        old_small: current.small_file_name,
        new_small,
        new_jpeg,
        canonical_jpeg: format!("{stem}.draft.poster.jpg"),
    })
}

async fn retire_new(state: &AppState, revision: &Revision) -> Result<(), ApiError> {
    let keys = vec![revision.new_small.clone(), revision.new_jpeg.clone()];
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    sqlx::query!(
        "UPDATE video_draft_outputs SET retired = true
         WHERE media_id = $1 AND file_name = ANY($2)",
        revision.media_id,
        &keys,
    )
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "the fenced file writes and atomic pointer swap form one auditable transaction"
)]
async fn finalize(
    state: &AppState,
    revision: Revision,
    native: media_processing::ProcessedImage,
    jpeg: media_processing::ProcessedImage,
    alt: String,
) -> Result<(), ApiError> {
    let native_size = i64::try_from(native.bytes.len()).unwrap_or(i64::MAX);
    let jpeg_size = i64::try_from(jpeg.bytes.len()).unwrap_or(i64::MAX);
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let attempt = async {
        let current = sqlx::query!(
            r#"SELECT m.small_file_name AS "small_file_name!", p.file_name AS "jpeg_file?"
               FROM video_drafts d JOIN media_attachments m ON m.id = d.media_id
               LEFT JOIN video_compatibility_posters p ON p.media_id = m.id
               WHERE d.media_id = $1 AND d.account_id = $2 AND m.account_id = $2
                 AND d.generation = $3 AND d.state = 'ready'
                 AND d.expires_at > clock_timestamp() AND m.status_id IS NULL
                 AND m.processing = 'complete' AND m.small_file_name IS NOT NULL
               FOR UPDATE OF m, d"#,
            revision.media_id,
            revision.owner,
            revision.generation,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?
        .ok_or(ApiError::Conflict(
            "The video draft changed before its poster was saved".into(),
        ))?;
        if current.small_file_name != revision.old_small {
            return Err(ApiError::Conflict(
                "The video poster was changed by another request".into(),
            ));
        }
        state
            .media
            .put(&revision.new_small, native.bytes)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        state
            .media
            .put(&revision.new_jpeg, jpeg.bytes)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let width = i32::try_from(native.width).unwrap_or(i32::MAX);
        let height = i32::try_from(native.height).unwrap_or(i32::MAX);
        sqlx::query!(
            "UPDATE media_attachments SET small_file_name = $3, small_width = $4,
               small_height = $5, thumbnail_file_size = $6, blurhash = NULL
             WHERE id = $1 AND account_id = $2",
            revision.media_id,
            revision.owner,
            revision.new_small,
            width,
            height,
            native_size,
        )
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        sqlx::query!(
            "INSERT INTO video_compatibility_posters
               (media_id, file_name, width, height, file_size, cleanup_job_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (media_id) DO UPDATE SET file_name = EXCLUDED.file_name,
               width = EXCLUDED.width, height = EXCLUDED.height,
               file_size = EXCLUDED.file_size, cleanup_job_id = EXCLUDED.cleanup_job_id",
            revision.media_id,
            revision.new_jpeg,
            i32::try_from(jpeg.width).unwrap_or(i32::MAX),
            i32::try_from(jpeg.height).unwrap_or(i32::MAX),
            jpeg_size,
            id::next(),
        )
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        sqlx::query!(
            "UPDATE video_drafts SET poster_alt = $2 WHERE media_id = $1",
            revision.media_id,
            alt,
        )
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        let old_keys = vec![
            revision.old_small.clone(),
            current
                .jpeg_file
                .unwrap_or_else(|| revision.canonical_jpeg.clone()),
        ];
        sqlx::query!(
            "UPDATE video_draft_outputs SET retired = true
             WHERE media_id = $1 AND file_name = ANY($2)",
            revision.media_id,
            &old_keys,
        )
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        media_cleanup::enqueue_many(&mut *tx, &old_keys).await?;
        Ok(())
    }
    .await;
    if let Err(error) = attempt {
        tx.rollback().await.map_err(plamenu_db::DbError::from)?;
        retire_new(state, &revision).await?;
        return Err(error);
    }
    // A commit transport failure has an uncertain outcome: leave the new
    // journaled bytes untouched for reconciliation, never delete them here.
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}
