//! Finite, seekable no-JavaScript rewind clips for local live broadcasts.
//!
//! A clip is a five-minute stream-copy of the already recorded fragmented MP4,
//! never a second encode. Requests for the same broadcast/minute/slice share
//! one bounded build and then ordinary HTTP byte ranges. The clip cache lives
//! inside the generation's scratch directory and disappears with that session.

use std::collections::HashMap;
use std::path::{Path as FilePath, PathBuf};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use super::media::{ByteRange, parse_range};
use crate::{error::ApiError, state::AppState};

pub(crate) const CLIP_SECONDS: i32 = 300;
const MAX_CLIP_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const CACHE_BUCKET_SECONDS: i64 = 60;
const CACHE_BUCKET_LIFETIME: i64 = 10;
const BUILD_TIMEOUT: Duration = Duration::from_secs(15);
const HEADER_WAIT: Duration = Duration::from_secs(10);
static BUILDS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn current_bucket() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp() / CACHE_BUCKET_SECONDS
}

pub(crate) fn clip_url(media_id: i64, bucket: i64, slice: i32) -> String {
    format!("/media/live/{media_id}/rewind/{bucket}/{slice}.mp4")
}

pub async fn clip(
    State(state): State<AppState>,
    Path((media_id, bucket, name)): Path<(i64, i64, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let Some(live) = plamenu_db::native_live::public_dvr(&state.pool, media_id).await? else {
        return Err(ApiError::NotFound);
    };
    let slice = name
        .strip_suffix(".mp4")
        .and_then(|value| value.parse::<i32>().ok())
        .ok_or(ApiError::NotFound)?;
    let now = current_bucket();
    if !crate::routes::native_live_hls::valid_uuid(&live.session_id)
        || slice < 0
        || slice >= live.window_seconds / CLIP_SECONDS
        || bucket > now
        || now.saturating_sub(bucket) > CACHE_BUCKET_LIFETIME
    {
        return Err(ApiError::NotFound);
    }
    let preferred =
        super::native_live_hls::playlist_directory(&state, media_id, &live.session_id).await;
    let floor = state.config.native_video.min_free_gib * crate::config::NativeVideoConfig::GIB
        + MAX_CLIP_BYTES;
    let primary = super::native_live_hls::primary_directory(&state, media_id, &live.session_id)
        .filter(|path| {
            preferred.as_ref() == Some(path)
                && matches!(crate::storage::available_bytes(path), Ok(free) if free >= floor)
        });
    let session_dir = primary
        .clone()
        .or_else(|| state.media.live_replica_dir(media_id, &live.session_id))
        .ok_or(ApiError::NotFound)?;
    let cache_dir = session_dir.join("rewind");
    let output = cache_dir.join(format!("{bucket}-{slice}.mp4"));
    if !valid_clip(&output).await {
        let identity = PathBuf::from(format!("{media_id}/{}", live.session_id));
        let build = build_lock(&identity).await;
        let _guard = tokio::time::timeout(Duration::from_secs(20), build.lock())
            .await
            .map_err(|_| {
                ApiError::ServiceUnavailable(
                    "A rewind clip is already being prepared; try again shortly".into(),
                )
            })?;
        if !valid_clip(&output).await {
            // An old URL may continue to serve its existing snapshot for a
            // short seek window, but must never generate fresh historic bytes.
            if bucket < now.saturating_sub(1) {
                return Err(ApiError::NotFound);
            }
            let _reservation = state
                .video_resources
                .try_reserve_temporary(2 * MAX_CLIP_BYTES)
                .ok_or_else(|| {
                    ApiError::ServiceUnavailable(
                        "Native media processing is busy; try again shortly".into(),
                    )
                })?;
            let source = rewind_input(
                &state,
                media_id,
                &live.session_id,
                slice,
                primary.as_deref(),
            )
            .await?;
            build_clip(
                &state,
                &session_dir,
                &source.path,
                &cache_dir,
                &output,
                (bucket, if source.pinned.is_some() { 0 } else { slice }),
                source.pinned.is_none(),
            )
            .await?;
        }
    }
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await?
        || !plamenu_db::native_live::public_dvr(&state.pool, media_id)
            .await?
            .is_some_and(|current| {
                current.session_id == live.session_id
                    && slice < current.window_seconds / CLIP_SECONDS
            })
    {
        return Err(ApiError::NotFound);
    }
    serve_clip(&output, &headers).await
}

struct RewindInput {
    path: PathBuf,
    pinned: Option<crate::storage::LiveRewindSource>,
}

async fn rewind_input(
    state: &AppState,
    media_id: i64,
    session: &str,
    slice: i32,
    primary: Option<&FilePath>,
) -> Result<RewindInput, ApiError> {
    let recording = primary.map(|path| path.join("replay.mp4"));
    let primary_ready = if let Some(path) = &recording {
        tokio::fs::symlink_metadata(path)
            .await
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink() && meta.len() > 0)
    } else {
        false
    };
    let pinned = if primary_ready {
        None
    } else {
        state
            .media
            .live_rewind_source(media_id, session.to_owned(), slice)
            .await
            .map_err(|error| {
                tracing::warn!(%error, media = media_id, "replica rewind unavailable");
                ApiError::ServiceUnavailable(
                    "The replica rewind is not ready; try again shortly".into(),
                )
            })?
    };
    let path = if let Some(snapshot) = &pinned {
        snapshot.playlist.clone()
    } else {
        let recording = recording.ok_or_else(|| {
            ApiError::ServiceUnavailable("No complete live rewind source is available".into())
        })?;
        wait_for_recording_header(&recording).await?;
        recording
    };
    Ok(RewindInput { path, pinned })
}

async fn build_lock(output: &FilePath) -> Arc<Mutex<()>> {
    let mut builds = BUILDS.lock().await;
    builds.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = builds.get(output).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    builds.insert(output.to_path_buf(), Arc::downgrade(&lock));
    lock
}

async fn valid_clip(path: &FilePath) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok_and(|meta| {
        meta.is_file()
            && !meta.file_type().is_symlink()
            && (1024..=MAX_CLIP_BYTES).contains(&meta.len())
    })
}

async fn build_clip(
    state: &AppState,
    session_dir: &FilePath,
    source: &FilePath,
    cache_dir: &FilePath,
    output: &FilePath,
    selection: (i64, i32),
    from_end: bool,
) -> Result<(), ApiError> {
    let (bucket, slice) = selection;
    let free = crate::storage::available_bytes(session_dir)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let floor = state.config.native_video.min_free_gib * crate::config::NativeVideoConfig::GIB;
    if free < floor.saturating_add(MAX_CLIP_BYTES) {
        return Err(ApiError::ServiceUnavailable(
            "Not enough free space for a live rewind clip".into(),
        ));
    }
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let dir_meta = tokio::fs::symlink_metadata(cache_dir)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if !dir_meta.is_dir() || dir_meta.file_type().is_symlink() {
        return Err(ApiError::NotFound);
    }
    prune(cache_dir, current_bucket(), None).await?;
    let part = cache_dir.join(format!("{bucket}-{slice}.part"));
    let _ = tokio::fs::remove_file(&part).await;
    let mut ready = false;
    for _ in 0..3 {
        let mut result = remux(state, source, &part, slice, from_end).await;
        if result.is_err() && slice == 0 && from_end {
            let _ = tokio::fs::remove_file(&part).await;
            result = remux(state, source, &part, slice, false).await;
        }
        if result.is_ok() && valid_clip(&part).await {
            ready = true;
            break;
        }
        let _ = tokio::fs::remove_file(&part).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    if !ready {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(ApiError::ServiceUnavailable(
            "The rewind clip is not ready yet".into(),
        ));
    }
    tokio::fs::rename(&part, output)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    prune(cache_dir, current_bucket(), Some(output)).await?;
    Ok(())
}

async fn wait_for_recording_header(source: &FilePath) -> Result<(), ApiError> {
    let deadline = tokio::time::Instant::now() + HEADER_WAIT;
    loop {
        if let Ok(meta) = tokio::fs::symlink_metadata(source).await {
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(ApiError::NotFound);
            }
            if let Ok(mut file) = tokio::fs::File::open(source).await {
                let mut header = [0_u8; 4096];
                if let Ok(size) = file.read(&mut header).await
                    && header[..size].windows(4).any(|atom| atom == b"moov")
                {
                    return Ok(());
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ApiError::ServiceUnavailable(
                "The live recording is still starting".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn remux(
    state: &AppState,
    source: &FilePath,
    output: &FilePath,
    slice: i32,
    from_end: bool,
) -> Result<(), ApiError> {
    let mut command = tokio::process::Command::new(&state.config.ffmpeg_path);
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if from_end {
        command.args(["-sseof", &format!("-{}", (slice + 1) * CLIP_SECONDS)]);
    } else {
        command.args(["-ss", "0"]);
    }
    command
        .arg("-i")
        .arg(source)
        .args([
            "-t",
            "300",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-fs",
            &MAX_CLIP_BYTES.to_string(),
            "-f",
            "mp4",
        ])
        .arg(output)
        .kill_on_drop(true);
    let result = tokio::time::timeout(BUILD_TIMEOUT, command.output())
        .await
        .map_err(|_| ApiError::ServiceUnavailable("The rewind clip took too long".into()))?
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if !result.status.success() {
        tracing::debug!(stderr = %String::from_utf8_lossy(&result.stderr), "live rewind remux failed");
        return Err(ApiError::ServiceUnavailable(
            "The rewind clip is not ready yet".into(),
        ));
    }
    Ok(())
}

async fn prune(cache_dir: &FilePath, now: i64, keep: Option<&FilePath>) -> Result<(), ApiError> {
    let mut entries = tokio::fs::read_dir(cache_dir)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut clips: Vec<(i64, PathBuf, u64)> = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((raw_bucket, tail)) = name.split_once('-') else {
            continue;
        };
        let Some(raw_slice) = tail.strip_suffix(".mp4") else {
            continue;
        };
        if raw_slice.parse::<i32>().is_err() {
            continue;
        }
        let Ok(bucket) = raw_bucket.parse::<i64>() else {
            continue;
        };
        let path = entry.path();
        let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !meta.is_file() || meta.file_type().is_symlink() {
            continue;
        }
        if now.saturating_sub(bucket) > CACHE_BUCKET_LIFETIME && keep != Some(path.as_path()) {
            let _ = tokio::fs::remove_file(path).await;
        } else {
            clips.push((bucket, path, meta.len()));
        }
    }
    clips.sort_by_key(|(bucket, _, _)| *bucket);
    let mut total: u64 = clips.iter().map(|(_, _, len)| len).sum();
    for (_, path, len) in clips {
        if total <= MAX_CACHE_BYTES {
            break;
        }
        if keep == Some(path.as_path()) {
            continue;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            total -= len;
        }
    }
    Ok(())
}

async fn serve_clip(path: &FilePath, headers: &HeaderMap) -> Result<Response, ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let total = file.metadata().await.map_err(|_| ApiError::NotFound)?.len();
    let common = [
        (header::CONTENT_TYPE, "video/mp4".to_owned()),
        (header::CACHE_CONTROL, "private, no-store".to_owned()),
        (header::ACCEPT_RANGES, "bytes".to_owned()),
    ];
    match parse_range(headers.get(header::RANGE), total) {
        ByteRange::Full => Ok((
            common,
            [(header::CONTENT_LENGTH, total.to_string())],
            Body::from_stream(ReaderStream::new(file.take(total))),
        )
            .into_response()),
        ByteRange::Partial(start, end) => {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|error| ApiError::Internal(Box::new(error)))?;
            let len = end - start + 1;
            Ok((
                StatusCode::PARTIAL_CONTENT,
                common,
                [
                    (header::CONTENT_LENGTH, len.to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    ),
                ],
                Body::from_stream(ReaderStream::new(file.take(len))),
            )
                .into_response())
        }
        ByteRange::Unsatisfiable => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            common,
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
        )
            .into_response()),
    }
}
