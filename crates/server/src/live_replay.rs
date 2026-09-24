//! Live replay finalization. The live encoder already produced H.264
//! and AAC for HLS; the VOD path normally remuxes those packets, then creates
//! one poster. No per-viewer work or second live encode is introduced.

use std::path::Path;
use std::time::Duration;

use plamenu_db::native_live::{self, ActiveSession, Replay};

use crate::{AppState, error::ApiError, media_limits::FileSizeLimit};

#[allow(
    clippy::too_many_lines,
    reason = "fenced replay conversion and crash-safe publication"
)]
pub async fn finalize(
    state: &AppState,
    session: &ActiveSession,
    recording: &Path,
) -> Result<bool, ApiError> {
    let source_bytes = tokio::fs::metadata(recording)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
        .len();
    if source_bytes == 0 || source_bytes > state.config.native_video.max_upload_bytes() {
        return Err(ApiError::Unprocessable(
            "Live replay exceeds the video size limit".into(),
        ));
    }
    let launcher = crate::media_limits::running_executable()
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let output_limit = state.config.native_video.output_limit(source_bytes);
    let mut slot = None;
    for _ in 0..300 {
        slot = state.video_resources.try_reserve(
            FileSizeLimit {
                launcher: launcher.clone(),
                bytes: output_limit,
            },
            source_bytes,
        );
        if slot.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let _slot = slot.ok_or_else(|| ApiError::Conflict("Video processing is busy".into()))?;
    let free = crate::storage::available_bytes(&state.config.media_dir)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let required = state.config.native_video.min_free_gib * crate::config::NativeVideoConfig::GIB
        + state.video_resources.usage().1;
    if free < required {
        return Err(ApiError::Unprocessable(
            "Not enough free space to process the live replay".into(),
        ));
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let mut params = crate::media_transcode::TranscodeParams::from_settings(&settings);
    params.max_av_bytes =
        usize::try_from(state.config.native_video.max_upload_bytes()).unwrap_or(usize::MAX);
    params.file_size_limit = Some(FileSizeLimit {
        launcher,
        bytes: output_limit,
    });
    let processed =
        crate::media_transcode::process_native_video_file(&state.config, recording, &params, 0.0)
            .await?;
    if processed.content_type != "video/mp4"
        || processed
            .duration
            .is_none_or(|seconds| !seconds.is_finite() || seconds <= 0.0)
    {
        return Err(ApiError::Unprocessable(
            "Live replay has no decodable video".into(),
        ));
    }
    let small = processed
        .small
        .as_ref()
        .ok_or_else(|| ApiError::Unprocessable("Live replay has no poster frame".into()))?;
    // PeerTube 8.3 rejects AVIF Video icons. Make the replay's one stored
    // preview a compatible, padded JPEG so the first-party client and peers
    // share a valid poster without an extra uncharged compatibility file.
    let poster = crate::media_processing::process_video_poster_blocking(
        small.bytes.clone(),
        crate::media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path),
    )
    .await?;
    // Recurring sessions need immutable episode keys across crash retries.
    // The session UUID is created by PostgreSQL and validated before recovery.
    let stem = if session.permanent {
        format!(
            "{}.{}",
            session.media_id,
            session.session_id.replace('-', "")
        )
    } else {
        crate::media_processing::storage_stem(session.media_id)
    };
    let file_name = format!("{stem}.livereplay.mp4");
    let poster_name = format!("{stem}.livereplay.small.jpg");
    let poster_size = i64::try_from(poster.bytes.len()).unwrap_or(i64::MAX);
    let keys = vec![file_name.clone(), poster_name.clone()];
    if !native_live::reserve_replay(
        &state.pool,
        session,
        &keys,
        &[processed.file_size, poster_size],
        &state.config.native_video.storage_limits(),
    )
    .await?
    {
        return Err(ApiError::Unprocessable(
            "Live replay cannot fit the video storage quota".into(),
        ));
    }
    let result = async {
        state
            .media
            .put_file(&file_name, &processed.file_path)
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        state
            .media
            .put(&poster_name, poster.bytes.clone())
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        let replay = Replay {
            file_name: &file_name,
            poster_name: &poster_name,
            file_size: processed.file_size,
            poster_size,
            width: processed.width,
            height: processed.height,
            poster_width: i32::try_from(poster.width).unwrap_or(i32::MAX),
            poster_height: i32::try_from(poster.height).unwrap_or(i32::MAX),
            duration: processed.duration.unwrap_or_default(),
            frame_rate: processed.frame_rate.as_deref(),
            bitrate: processed.bitrate,
            blurhash: processed.blurhash.as_deref(),
        };
        if session.permanent {
            let media_id = native_live::prepare_recurring_replay(&state.pool, session, &replay)
                .await?
                .ok_or_else(|| ApiError::Conflict("Recurring replay session changed".into()))?;
            let publication = native_live::recurring_replay_publication(&state.pool, session)
                .await?
                .ok_or_else(|| ApiError::Conflict("Recurring replay already published".into()))?;
            if publication.media_id != media_id {
                return Err(ApiError::Conflict("Recurring replay media changed".into()));
            }
            let source_link = format!(
                "https://{}/@{}/{}",
                state.config.domain, publication.username, session.status_id
            );
            let text = format!(
                "{}\n\nReplay of {source_link}",
                publication.text.chars().take(9000).collect::<String>()
            );
            let params = crate::actions::PostParams {
                username: &publication.username,
                text: &text,
                visibility: &publication.visibility,
                media_ids: &[media_id],
                language: publication.language.as_deref(),
                content_type: crate::compose::PostFormat::from_media_type(
                    &publication.content_type,
                ),
                title: Some(&publication.title),
                ..Default::default()
            };
            crate::actions::post_recurring_live_replay(
                state,
                params,
                publication.channel_id,
                &session.session_id,
            )
            .await?;
            Ok(true)
        } else {
            native_live::publish_replay(&state.pool, session, replay)
                .await
                .map_err(ApiError::from)
        }
    }
    .await;
    if !matches!(result, Ok(true)) {
        // The cleanup journal is atomically removed when publication commits.
        // If commit succeeded but its reply was lost, this no-ops on its keys.
        native_live::abandon_replay(&state.pool, &keys).await?;
    }
    result
}
