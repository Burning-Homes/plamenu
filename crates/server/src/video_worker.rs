//! One durable native-video processing slot. The scheduler must reserve a
//! scratch/output budget before calling this; admission and UI wiring are
//! deliberately separate. No transaction is held while probing or encoding.

use std::time::Duration;
use tokio::task::JoinHandle;

use plamenu_db::{media, native_video_hls, video_draft};

use crate::{
    AppState,
    error::ApiError,
    media_transcode::{PackagedHls, PackagedStoryboard, ProcessedAv},
    video_resources::Reservation,
};

const RENEW_EVERY: Duration = Duration::from_secs(5);
const STORE_WARNING: Duration = Duration::from_secs(30);

/// Remove only our own abandoned scratch names at process startup, before
/// any media worker or upload handler can create a live one. Unknown names and
/// symlinks are untouched. Ordinary and native A/V share the `av-` scratch
/// prefix, so both benefit after an unclean shutdown.
pub async fn scavenge_crash_scratch(media_dir: &std::path::Path) -> std::io::Result<usize> {
    let root = media_dir.join("tmp");
    let metadata = match tokio::fs::symlink_metadata(&root).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(0);
    }
    let mut entries = tokio::fs::read_dir(&root).await?;
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let kind = entry.file_type().await?;
        let random_suffix = |prefix: &str| {
            name.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() >= 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        };
        if random_suffix("video-upload-") && kind.is_file() {
            tokio::fs::remove_file(entry.path()).await?;
            removed += 1;
        } else if random_suffix("av-") && kind.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Account old crash-attempt files without racing a final write. Runs before
/// normal admission on upgrade; unknown sizes otherwise keep admission closed.
pub async fn recover_storage_accounting(state: &AppState) -> Result<usize, ApiError> {
    let mut recovered = plamenu_db::video_storage::recover_missing_posters(&state.pool).await?;
    let pending = plamenu_db::video_storage::unmeasured(&state.pool).await?;
    for (key, media_id) in pending {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        sqlx::query!(
            "SELECT id FROM media_attachments WHERE id = $1 FOR UPDATE",
            media_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        let active = sqlx::query_scalar!("SELECT EXISTS (SELECT 1 FROM video_draft_outputs o
            JOIN video_drafts d ON d.media_id = o.media_id AND d.generation = o.generation
            WHERE o.file_name = $1 AND d.state = 'processing' AND d.lease_until > clock_timestamp())", key)
            .fetch_one(&mut *tx).await.map_err(plamenu_db::DbError::from)?.unwrap_or(true);
        if active {
            continue;
        }
        let bytes = match state.media.open(&key).await {
            Ok(file) => i64::try_from(file.len).unwrap_or(i64::MAX),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(ApiError::Internal(Box::new(error))),
        };
        plamenu_db::video_storage::measure(&mut tx, &key, bytes).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        recovered += 1;
    }
    Ok(recovered)
}

fn lost_claim() -> ApiError {
    ApiError::Conflict("Video processing was cancelled or its lease expired".into())
}

/// Claim only one job for an already reserved slot. This temporary budget is
/// NOT a substitute for durable stored-media quotas or disk free-space checks.
pub async fn run_one(
    state: &AppState,
    reservation: Reservation,
    storage_limits: &plamenu_db::video_storage::Limits,
) -> Result<bool, ApiError> {
    let Some(claim) = video_draft::claim(&state.pool).await? else {
        return Ok(false);
    };
    if u64::try_from(claim.source_bytes).unwrap_or(u64::MAX) > reservation.source_ceiling {
        video_draft::defer(&state.pool, &claim).await?;
        return Ok(false);
    }
    let result = process(state, &claim, reservation, storage_limits).await;
    if result.is_err() {
        // Failure never discards the original. On an uncertain commit or a
        // lost lease this is a fenced no-op, not a rollback of a newer result.
        video_draft::fail(&state.pool, &claim).await?;
    }
    result.map(|()| true)
}

async fn process(
    state: &AppState,
    claim: &video_draft::Claim,
    reservation: Reservation,
    storage_limits: &plamenu_db::video_storage::Limits,
) -> Result<(), ApiError> {
    if !video_draft::retire_previous(&state.pool, claim).await? {
        return Err(lost_claim());
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let mut params = crate::media_transcode::TranscodeParams::from_settings(&settings);
    (params.force_reencode, params.window) =
        crate::video_source_edit::processing(state, claim.media_id).await?;
    params.file_size_limit = Some(reservation.limit.clone());
    params.max_av_bytes = usize::try_from(reservation.source_ceiling).unwrap_or(usize::MAX);
    let stem = crate::media_processing::storage_stem(claim.media_id);
    let cancellation = tokio_util::sync::CancellationToken::new();
    let encode = crate::video_offload::encode(
        state,
        &claim.source_file,
        &params,
        claim.poster_time_seconds,
        &stem,
        &cancellation,
    );
    tokio::pin!(encode);
    let mut renew = tokio::time::interval(RENEW_EVERY);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut processed, hls, storyboard) = loop {
        tokio::select! {
            result = &mut encode => break result?,
            _ = renew.tick() => {
                if !video_draft::renew(&state.pool, claim).await? {
                    cancellation.cancel();
                    // Give the one admitted remote attempt a finite orderly
                    // DELETE window. Server crash/aborted futures still use the
                    // remote lease; no unbounded async-Drop queue is introduced.
                    let _ = tokio::time::timeout(Duration::from_secs(3), &mut encode).await;
                    return Err(lost_claim());
                }
            }
        }
    };
    // Intentional silent films are still native Video, never looping gifv.
    processed.kind = "video";
    let main = format!("{stem}.draft.mp4");
    let small = format!("{stem}.draft.small.avif");
    let small_size = i64::try_from(
        processed
            .small
            .as_ref()
            .expect("poster checked")
            .bytes
            .len(),
    )
    .unwrap_or(i64::MAX);
    let mut hls_sizes: Vec<_> = hls
        .iter()
        .map(|item| (item.file_name.clone(), item.file_size))
        .collect();
    hls_sizes.extend(
        hls.iter()
            .filter_map(|r| r.progressive.as_ref())
            .map(|p| (p.metadata.file_name.clone(), p.metadata.size_bytes)),
    );
    hls_sizes.push((storyboard.file_name.clone(), storyboard.file_size));
    if !video_draft::reserve_outputs_with_hls(
        &state.pool,
        claim,
        &main,
        &small,
        [processed.file_size, small_size],
        &hls_sizes,
        storage_limits,
    )
    .await?
    {
        return Err(lost_claim());
    }
    // A final fresh lease leaves ample time for this bounded rename/commit
    // window. If the commit outcome is uncertain, keep journaled files: only
    // a later fenced retry/cancellation may enqueue their cleanup.
    if !video_draft::renew(&state.pool, claim).await? {
        return Err(lost_claim());
    }
    // Filesystem operations can continue on Tokio's blocking pool after their
    // future is dropped. Own this short finalization in a task so cancelling
    // the caller cannot release the DB fence ahead of an in-flight write.
    // A slow store is reported, not cancelled. Process-crash leftovers remain
    // private and journaled for retry/cleanup. Move the resource reservation
    // too, so a detached finalizer still occupies its slot and scratch budget.
    let state = state.clone();
    let claim = claim.clone();
    let media_id = claim.media_id;
    let mut finalize = tokio::spawn(async move {
        let _reservation = reservation;
        let result = store(&state, &claim, &processed, &hls, &storyboard, &main, &small).await;
        // Remove residual poster/hash scratch before releasing its budget.
        drop(processed);
        if result.is_err() {
            video_draft::fail(&state.pool, &claim).await?;
        }
        result
    });
    let result = tokio::select! {
        result = &mut finalize => result,
        () = tokio::time::sleep(STORE_WARNING) => {
            tracing::warn!(media_id, "video final storage is slow; retaining its fence");
            finalize.await
        }
    };
    result.map_err(|error| ApiError::Internal(Box::new(error)))?
}

/// Drain one queue lane until shutdown. Resource reservation happens before
/// the durable claim; an unavailable disk or a full pool leaves jobs queued.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let launcher = match crate::media_limits::running_executable() {
            Ok(path) => path,
            Err(error) => {
                tracing::error!(%error, "native video worker cannot locate its executable");
                return;
            }
        };
        let config = state.config.native_video;
        tracing::info!("native video worker started");
        let mut last_recovery = tokio::time::Instant::now() - Duration::from_secs(60);
        let mut accounting_ready = false;
        loop {
            if last_recovery.elapsed() >= Duration::from_secs(60) {
                match recover_storage_accounting(&state).await {
                    Ok(_) => match plamenu_db::video_storage::unmeasured(&state.pool).await {
                        Ok(pending) => {
                            accounting_ready = pending.is_empty();
                            if !accounting_ready {
                                tracing::warn!(
                                    count = pending.len(),
                                    "native video accounting still has unmeasured files; keeping jobs queued"
                                );
                            }
                        }
                        Err(error) => {
                            accounting_ready = false;
                            tracing::error!(%error, "native video accounting check failed");
                        }
                    },
                    Err(error) => {
                        accounting_ready = false;
                        tracing::error!(%error, "native video accounting recovery failed");
                    }
                }
                last_recovery = tokio::time::Instant::now();
            }
            if !accounting_ready {
                if !crate::workers::pause(&state, Duration::from_secs(2)).await {
                    return;
                }
                continue;
            }
            let attempted = match video_draft::next_source_bytes(&state.pool).await {
                Ok(Some(bytes)) if bytes > 0 => {
                    let source = u64::try_from(bytes).unwrap_or(u64::MAX);
                    let limit = crate::media_limits::FileSizeLimit {
                        launcher: launcher.clone(),
                        bytes: config.output_limit(source),
                    };
                    if let Some(slot) = state.video_resources.try_reserve(limit, source) {
                        let free = crate::storage::available_bytes(&state.config.media_dir);
                        let required = config.min_free_gib * crate::config::NativeVideoConfig::GIB
                            + state.video_resources.usage().1;
                        match free {
                            Ok(free) if free >= required => {
                                match run_one(&state, slot, &config.storage_limits()).await {
                                    Ok(worked) => worked,
                                    Err(error) => {
                                        tracing::warn!(error = %error.chain(), "native video job failed");
                                        true
                                    }
                                }
                            }
                            Ok(_) => false,
                            Err(error) => {
                                tracing::warn!(%error, "native video free-space measurement failed");
                                false
                            }
                        }
                    } else {
                        false
                    }
                }
                Ok(_) => false,
                Err(error) => {
                    tracing::error!(%error, "native video queue lookup failed");
                    false
                }
            };
            if !attempted && !crate::workers::pause(&state, Duration::from_secs(2)).await {
                return;
            }
            if state.shutdown.is_cancelled() {
                return;
            }
        }
    })
}

async fn store(
    state: &AppState,
    claim: &video_draft::Claim,
    processed: &ProcessedAv,
    hls: &[PackagedHls],
    storyboard: &PackagedStoryboard,
    main: &str,
    small: &str,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !video_draft::lock_claim(&mut tx, claim).await? {
        return Err(lost_claim());
    }
    let frame = processed
        .small
        .as_ref()
        .ok_or_else(|| ApiError::Unprocessable("Video has no poster".into()))?;
    state
        .media
        .put_file(main, &processed.file_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    state
        .media
        .put(small, frame.bytes.clone())
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    for rendition in hls {
        state
            .media
            .put_file(&rendition.file_name, &rendition.file_path)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        if let Some(p) = &rendition.progressive {
            state
                .media
                .put_file(&p.metadata.file_name, &p.file_path)
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
        }
    }
    state
        .media
        .put_file(&storyboard.file_name, &storyboard.file_path)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let done = media::complete_processing(
        &mut *tx,
        media::ProcessedMedia {
            media_id: claim.media_id,
            file_name: main,
            content_type: processed.content_type,
            kind: Some("video"),
            width: processed.width,
            height: processed.height,
            blurhash: processed.blurhash.as_deref(),
            duration: processed.duration,
            frame_rate: processed.frame_rate.as_deref(),
            bitrate: processed.bitrate,
            small: Some(media::SmallStyle {
                file_name: small,
                width: i32::try_from(frame.width).unwrap_or(i32::MAX),
                height: i32::try_from(frame.height).unwrap_or(i32::MAX),
            }),
        },
    )
    .await?;
    if done.is_none() {
        return Err(lost_claim());
    }
    media::set_file_sizes(
        &mut *tx,
        claim.media_id,
        processed.file_size,
        Some(i64::try_from(frame.bytes.len()).unwrap_or(i64::MAX)),
    )
    .await?;
    let actual_poster_time =
        crate::media_transcode::poster_seek_seconds(claim.poster_time_seconds, processed.duration);
    if sqlx::query!(
        "UPDATE video_drafts SET poster_time_seconds = $3
         WHERE media_id = $1 AND generation = $2",
        claim.media_id,
        claim.generation,
        actual_poster_time,
    )
    .execute(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?
    .rows_affected()
        != 1
    {
        return Err(lost_claim());
    }
    store_hls_metadata(&mut tx, claim, hls).await?;
    plamenu_db::video_storyboards::replace(&mut tx, &storyboard_row(claim, storyboard)).await?;
    if !video_draft::mark_ready(&mut tx, claim).await? {
        return Err(lost_claim());
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(())
}

async fn store_hls_metadata(
    connection: &mut plamenu_db::PgConnection,
    claim: &video_draft::Claim,
    hls: &[PackagedHls],
) -> Result<(), ApiError> {
    let hls_rows = hls_rows(claim, hls);
    native_video_hls::replace(connection, claim.media_id, claim.generation, &hls_rows).await?;
    for rung in hls {
        if let Some(p) = &rung.progressive {
            native_video_hls::set_progressive(
                connection,
                claim.media_id,
                claim.generation,
                rung.height,
                &p.metadata,
            )
            .await?;
        }
    }
    Ok(())
}

fn storyboard_row(
    claim: &video_draft::Claim,
    board: &PackagedStoryboard,
) -> plamenu_db::video_storyboards::Storyboard {
    plamenu_db::video_storyboards::Storyboard {
        media_id: claim.media_id,
        generation: claim.generation,
        file_name: board.file_name.clone(),
        size_bytes: board.file_size,
        columns: board.columns,
        rows: board.rows,
        tile_count: board.count,
        interval_seconds: board.interval_seconds,
    }
}

fn hls_rows(claim: &video_draft::Claim, hls: &[PackagedHls]) -> Vec<native_video_hls::Rendition> {
    hls.iter()
        .map(|item| native_video_hls::Rendition {
            media_id: claim.media_id,
            generation: claim.generation,
            height: item.height,
            width: item.width,
            bandwidth: item.bandwidth,
            size_bytes: item.file_size,
            file_name: item.file_name.clone(),
            playlist: item.playlist.clone(),
            segment_sha256: item
                .segment_sha256
                .get(&item.file_name)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::scavenge_crash_scratch;

    #[tokio::test]
    async fn startup_scavenges_only_known_scratch_shapes() {
        let media = tempfile::tempdir().unwrap();
        let root = media.path().join("tmp");
        tokio::fs::create_dir(&root).await.unwrap();
        let abandoned = root.join("video-upload-ABC123");
        tokio::fs::write(&abandoned, b"source").await.unwrap();
        let av = root.join("av-ABC123");
        tokio::fs::create_dir(&av).await.unwrap();
        tokio::fs::write(av.join("output.mp4"), b"output")
            .await
            .unwrap();
        let unrelated = root.join("video-upload-readme.txt");
        tokio::fs::write(&unrelated, b"operator file")
            .await
            .unwrap();
        assert_eq!(scavenge_crash_scratch(media.path()).await.unwrap(), 2);
        assert!(!abandoned.exists());
        assert!(!av.exists());
        assert_eq!(tokio::fs::read(&unrelated).await.unwrap(), b"operator file");
        assert_eq!(scavenge_crash_scratch(media.path()).await.unwrap(), 0);
    }
}
