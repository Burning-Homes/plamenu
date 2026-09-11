//! The media transcoding worker — Mastodon's `PostProcessMediaWorker`:
//! v2 video/audio uploads answer 202 immediately and are transcoded here,
//! claimed from `media_processing_jobs` (single attempt; a failure marks
//! the attachment `failed`, which clients see as a 422 on
//! `GET /api/v1/media/{id}`).

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use plamenu_db::{account, account_media, media, media_fetch_failure};
use plamenu_federation::{FederationError, FetchedMedia};
use tokio::task::JoinHandle;

use crate::error::ApiError;
use crate::media_processing::{self, ProcessedImage};
use crate::media_transcode::ProcessedAv;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

const BATCH_SIZE: i64 = 4;
const IDLE_POLL: Duration = Duration::from_secs(1);

/// A remote download retries on failure (the origin may be briefly
/// unreachable) up to this many attempts, then gives up and leaves the row
/// serving the remote URL — never `failed` (Mastodon keeps the `remote_url`
/// and redownloads later).
const MAX_REMOTE_ATTEMPTS: i32 = 5;
/// Backoff base; attempt `n` waits `RETRY_BASE * 2^n` (capped).
const RETRY_BASE: Duration = Duration::from_mins(1);

/// Exponential backoff for a remote download's next attempt.
fn retry_delay(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.clamp(0, 6)).unwrap_or(0);
    RETRY_BASE.saturating_mul(1 << shift)
}

/// How long the media proxy remembers a failed on-demand fetch before trying
/// the origin again. A dead or IP-blocked remote image would otherwise be
/// re-fetched on every timeline render; within the cooldown the proxy fails
/// closed instantly (see [`plamenu_db::media_fetch_failure`]).
// `from_secs` (not the nightly-only `from_mins`) keeps this off the unstable
// `duration_constructors` feature; the pedantic lint suggests that unstable API.
#[allow(clippy::duration_suboptimal_units)]
const MEDIA_REFETCH_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Poster frames cached from a remote video's `thumbnail_remote_url` (the
/// video itself is on-demand) — Mastodon's 640px video `small` style.
const POSTER_MAX_EDGE: u32 = 640;

/// How long a play request waits for the A/V lane to finish warming the
/// video before answering with what exists (the origin URL for opted-in
/// viewers, a 404 otherwise). Short videos finish inside this window, so
/// the first tap simply buffers a little longer and then plays.
const AV_WARMUP_WAIT: Duration = Duration::from_secs(15);

/// In-process single-flight for on-demand fetches: concurrent proxy requests
/// for the same uncached entity coalesce into one download instead of N.
static INFLIGHT: LazyLock<Mutex<HashSet<(&'static str, i64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Marks `(kind, id)` as being fetched; [`None`] when another request
/// already is. Dropping the guard clears the mark.
struct InflightGuard {
    key: (&'static str, i64),
}

impl InflightGuard {
    fn begin(kind: &'static str, id: i64) -> Option<Self> {
        // The guard's own `Drop` locks `INFLIGHT`, so it must never be
        // created — or, as `then_some`'s eager argument, dropped — while
        // this statement's lock is held (froze the runtime, 2026-07-12).
        let inserted = INFLIGHT.lock_or_recover().insert((kind, id));
        inserted.then(|| Self { key: (kind, id) })
    }

    fn running(kind: &'static str, id: i64) -> bool {
        INFLIGHT.lock_or_recover().contains(&(kind, id))
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.lock_or_recover().remove(&self.key);
    }
}

/// After an on-demand proxy fetch, remembers a failure (so the next render
/// fails closed) or clears a stale one on success. `cached` is whether the
/// entity ended up with a stored file.
async fn note_proxy_fetch(pool: &plamenu_db::PgPool, kind: &str, id: i64, cached: bool) {
    let result = if cached {
        media_fetch_failure::clear(pool, kind, id).await
    } else {
        media_fetch_failure::record(pool, kind, id).await
    };
    if let Err(error) = result {
        tracing::debug!(kind, id, %error, "recording media fetch outcome failed");
    }
}

/// The result of storing a processed attachment: the delivery file name, the
/// small style for the database row, and the stored byte sizes for the admin
/// storage metrics (`media::set_file_sizes`).
pub struct StoredOutput {
    pub file_name: String,
    pub small: Option<(String, i32, i32)>,
    pub file_size: i64,
    pub thumbnail_file_size: Option<i64>,
}

/// Stores a processed video/audio file plus its poster frame, returning the
/// delivery file name, small style, and stored byte sizes.
pub async fn store_av_output(
    state: &AppState,
    media_id: i64,
    processed: &ProcessedAv,
) -> Result<StoredOutput, ApiError> {
    let stem = media_processing::storage_stem(media_id);
    let file_name = format!("{stem}.{}", processed.extension);
    let file_size = processed.file_size;
    // The transcode output lives on disk under `{media_dir}/tmp` — same
    // filesystem, so this is a rename, never a buffered copy.
    state
        .media
        .put_file(&file_name, &processed.file_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut thumbnail_file_size = None;
    let small = match &processed.small {
        Some(frame) => {
            // The poster frame is AVIF (see `process_av`).
            let small_name = format!("{stem}.small.avif");
            thumbnail_file_size = Some(byte_len(frame.bytes.len()));
            state
                .media
                .put(&small_name, frame.bytes.clone())
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Some((
                small_name,
                i32::try_from(frame.width).unwrap_or(i32::MAX),
                i32::try_from(frame.height).unwrap_or(i32::MAX),
            ))
        }
        None => None,
    };
    Ok(StoredOutput {
        file_name,
        small,
        file_size,
        thumbnail_file_size,
    })
}

/// Stores a processed image plus its small style, returning the delivery file
/// name, small style, and stored byte sizes (the image counterpart of
/// [`store_av_output`]). The small carries its own extension — the full and
/// preview renditions can be different formats (e.g. an AVIF preview of a
/// passthrough PNG original).
async fn store_image_output(
    state: &AppState,
    media_id: i64,
    processed: &ProcessedImage,
) -> Result<StoredOutput, ApiError> {
    let stem = media_processing::storage_stem(media_id);
    let file_name = format!("{stem}.{}", processed.extension);
    let file_size = byte_len(processed.bytes.len());
    state
        .media
        .put(&file_name, processed.bytes.clone())
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut thumbnail_file_size = None;
    let small = match &processed.small {
        Some(frame) => {
            let small_name = format!("{stem}.small.{}", frame.extension);
            thumbnail_file_size = Some(byte_len(frame.bytes.len()));
            state
                .media
                .put(&small_name, frame.bytes.clone())
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Some((
                small_name,
                i32::try_from(frame.width).unwrap_or(i32::MAX),
                i32::try_from(frame.height).unwrap_or(i32::MAX),
            ))
        }
        None => None,
    };
    Ok(StoredOutput {
        file_name,
        small,
        file_size,
        thumbnail_file_size,
    })
}

/// A byte length as a saturating `i64` (storage sizes never realistically
/// overflow, but the cast must be total).
fn byte_len(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

/// A passive, non-executable file's safe local representation. Unknown and
/// active types deliberately become `.bin`/octet-stream so a hostile remote
/// cannot make our own media origin execute HTML or SVG.
fn passive_file_format(content_type: &str) -> (&'static str, &'static str) {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "text/markdown" => ("text/markdown", "md"),
        "text/plain" => ("text/plain", "txt"),
        "application/pdf" => ("application/pdf", "pdf"),
        "application/zip" => ("application/zip", "zip"),
        _ => ("application/octet-stream", "bin"),
    }
}

/// Downloads a remote attachment and runs it through the matching cache path:
/// still images re-encode (+ small + blurhash), A/V goes through ffmpeg (which
/// re-detects gifv), and passive documents are stored as safe downloads. On
/// success the row gains a local file and its true kind/metadata.
#[allow(
    clippy::too_many_lines,
    reason = "one linear fetch → classify → process → store flow"
)]
async fn cache_remote(
    state: &AppState,
    media_id: i64,
    remote_url: &str,
    declared_content_type: &str,
) -> Result<(), ApiError> {
    let fetched = state
        .federation
        .fetch_media(remote_url)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;

    // Some media endpoints omit Content-Type or answer octet-stream despite a
    // useful AP `mediaType`. Prefer a meaningful response, otherwise retain
    // the signed object's declaration for classification.
    let response_type = fetched.content_type.split(';').next().unwrap_or("").trim();
    let effective_type = if response_type.is_empty()
        || response_type.eq_ignore_ascii_case("application/octet-stream")
    {
        declared_content_type
    } else {
        response_type
    };

    let settings = state.settings_cache.get(&state.pool).await?;
    // A remote animated GIF is kept (or re-encoded alpha-safe) instead of
    // being pushed through the gifv mp4 conversion: peers serve the origin
    // bytes, and re-encoding cost their pixel art its quality and rendered
    // transparency as a solid background (M39). Under the opt-in `gifv` mode
    // only opaque GIFs convert; `webp` re-encodes to animated WebP below.
    let animated_gif = media_processing::is_animated_gif(&fetched.bytes);
    let gif_mode = settings.media_remote_gif_handling.as_str();
    let gif_stays_image = animated_gif
        && (gif_mode == "keep"
            || gif_mode == "webp"
            || media_processing::gif_first_frame_has_alpha(&fetched.bytes));

    if media_processing::is_still_image(&fetched.bytes) || gif_stays_image {
        let full =
            media_processing::FullMedia::from_setting(&settings.media_remote_full_processing);
        let preview =
            media_processing::PreviewMedia::from_setting(&settings.media_preview_processing);
        let params =
            media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
        // For an animated GIF this passes the original through (the animated
        // guard overrides a re-encoding full mode) while still deriving the
        // first-frame preview and blurhash.
        let mut processed =
            media_processing::process_upload_blocking(fetched.bytes.clone(), full, preview, params)
                .await?;
        if animated_gif && gif_mode == "webp" {
            match crate::media_transcode::gif_to_animated_webp(&state.config, &fetched.bytes).await
            {
                Ok(webp) => {
                    processed.bytes = webp;
                    processed.content_type = "image/webp";
                    processed.extension = "webp";
                }
                // Best-effort: an ffmpeg without libwebp_anim keeps the GIF.
                Err(error) => {
                    tracing::warn!(%error, media = media_id, "animated WebP re-encode failed; keeping the GIF");
                }
            }
        }
        let stored = store_image_output(state, media_id, &processed).await?;
        media::complete_processing(
            &state.pool,
            media::ProcessedMedia {
                media_id,
                file_name: &stored.file_name,
                content_type: processed.content_type,
                kind: Some("image"),
                width: Some(i32::try_from(processed.width).unwrap_or(i32::MAX)),
                height: Some(i32::try_from(processed.height).unwrap_or(i32::MAX)),
                blurhash: processed.blurhash.as_deref(),
                small: stored
                    .small
                    .as_ref()
                    .map(|(name, width, height)| media::SmallStyle {
                        file_name: name,
                        width: *width,
                        height: *height,
                    }),
                duration: None,
                frame_rate: None,
                bitrate: None,
            },
        )
        .await?;
        media::set_file_sizes(
            &state.pool,
            media_id,
            stored.file_size,
            stored.thumbnail_file_size,
        )
        .await
        .ok();
    } else if effective_type.starts_with("audio/")
        || effective_type.starts_with("video/")
        // An opaque animated GIF in the opt-in `gifv` mode deliberately uses
        // the A/V transcoder even though its declared type is still image/gif.
        || animated_gif
    {
        let params = crate::media_transcode::TranscodeParams::from_settings(&settings);
        let processed =
            crate::media_transcode::process_av(&state.config, &fetched.bytes, &params).await?;
        let stored = store_av_output(state, media_id, &processed).await?;
        media::complete_processing(
            &state.pool,
            media::ProcessedMedia {
                media_id,
                file_name: &stored.file_name,
                content_type: processed.content_type,
                kind: Some(processed.kind),
                width: processed.width,
                height: processed.height,
                blurhash: processed.blurhash.as_deref(),
                small: stored
                    .small
                    .as_ref()
                    .map(|(name, width, height)| media::SmallStyle {
                        file_name: name,
                        width: *width,
                        height: *height,
                    }),
                duration: processed.duration,
                frame_rate: processed.frame_rate.as_deref(),
                bitrate: processed.bitrate,
            },
        )
        .await?;
        media::set_file_sizes(
            &state.pool,
            media_id,
            stored.file_size,
            stored.thumbnail_file_size,
        )
        .await
        .ok();
    } else {
        let (content_type, extension) = passive_file_format(effective_type);
        let file_size = byte_len(fetched.bytes.len());
        let file_name = format!("{}.{}", media_processing::storage_stem(media_id), extension);
        state
            .media
            .put(&file_name, fetched.bytes)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        media::complete_processing(
            &state.pool,
            media::ProcessedMedia {
                media_id,
                file_name: &file_name,
                content_type,
                kind: None,
                width: None,
                height: None,
                blurhash: None,
                small: None,
                duration: None,
                frame_rate: None,
                bitrate: None,
            },
        )
        .await?;
        media::set_file_sizes(&state.pool, media_id, file_size, None)
            .await
            .ok();
    }
    Ok(())
}

/// Why an on-demand A/V download did not produce a cached file.
enum AvCacheError {
    /// This video can never cache (over budget, refused by the origin,
    /// or a codec we will not re-encode): give up without retries.
    Permanent(String),
    /// Worth retrying with backoff (the origin may be briefly unreachable).
    Transient(ApiError),
}

impl AvCacheError {
    fn from_fetch(error: FederationError) -> Self {
        match error {
            FederationError::TooLarge(url) => {
                Self::Permanent(format!("{url}: over the video cache budget"))
            }
            FederationError::Status(code @ (401 | 403 | 404 | 410 | 451)) => {
                Self::Permanent(format!("origin answered {code}"))
            }
            other => Self::Transient(ApiError::Internal(Box::new(other))),
        }
    }
}

/// Downloads an on-demand remote video (plus its separated-audio companion
/// when the origin ships one) into scratch space, remuxes it into the
/// universally playable delivery mp4 and stores it. Streamed end to end:
/// memory use stays flat regardless of file size.
#[allow(
    clippy::too_many_lines,
    reason = "one linear download → remux → store flow"
)]
async fn cache_remote_av(state: &AppState, item: &media::Media) -> Result<(), AvCacheError> {
    let media_id = item.id;
    let remote_url = item
        .remote_url
        .as_deref()
        .ok_or_else(|| AvCacheError::Permanent("row has no remote url".into()))?;
    let settings = state
        .settings_cache
        .get(&state.pool)
        .await
        .map_err(|e| AvCacheError::Transient(e.into()))?;
    let budget = u64::try_from(settings.remote_video_max_mb)
        .unwrap_or(0)
        .saturating_mul(1024 * 1024);
    if budget == 0 {
        return Err(AvCacheError::Permanent(
            "remote video caching is off".into(),
        ));
    }

    // Scratch under the media root: the finished file moves into the store
    // by rename, and a huge download never lands in a RAM-backed /tmp.
    let scratch_root = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&scratch_root)
        .await
        .map_err(|e| AvCacheError::Transient(ApiError::Internal(Box::new(e))))?;
    let dir = tempfile::Builder::new()
        .prefix("av-")
        .tempdir_in(&scratch_root)
        .map_err(|e| AvCacheError::Transient(ApiError::Internal(Box::new(e))))?;

    let video_path = dir.path().join("video.bin");
    let fetched = state
        .federation
        .fetch_media_to_file(remote_url, &video_path, budget)
        .await
        .map_err(AvCacheError::from_fetch)?;
    let audio_path = match &item.remote_audio_url {
        Some(audio_url) => {
            let path = dir.path().join("audio.bin");
            state
                .federation
                .fetch_media_to_file(audio_url, &path, budget - fetched.bytes_written)
                .await
                .map_err(AvCacheError::from_fetch)?;
            Some(path)
        }
        None => None,
    };

    let params = crate::media_transcode::TranscodeParams::from_settings(&settings);
    let processed = crate::media_transcode::process_remote_av(
        &state.config,
        dir.path(),
        &video_path,
        audio_path.as_deref(),
        &params,
    )
    .await
    .map_err(|error| match error {
        // The codec/container gate: re-encoding hours of video is off the
        // table, so this input will never cache.
        ApiError::Unprocessable(reason) => AvCacheError::Permanent(reason),
        other => AvCacheError::Transient(other),
    })?;

    let stem = media_processing::storage_stem(media_id);
    let file_name = format!("{stem}.{}", processed.extension);
    state
        .media
        .put_file(&file_name, &processed.file_path)
        .await
        .map_err(|e| AvCacheError::Transient(ApiError::Internal(Box::new(e))))?;
    let mut thumbnail_file_size = None;
    let small = match &processed.small {
        Some(frame) => {
            let small_name = format!("{stem}.small.avif");
            thumbnail_file_size = Some(byte_len(frame.bytes.len()));
            state
                .media
                .put(&small_name, frame.bytes.clone())
                .await
                .map_err(|e| AvCacheError::Transient(ApiError::Internal(Box::new(e))))?;
            Some((
                small_name,
                i32::try_from(frame.width).unwrap_or(i32::MAX),
                i32::try_from(frame.height).unwrap_or(i32::MAX),
            ))
        }
        None => None,
    };
    media::complete_processing(
        &state.pool,
        media::ProcessedMedia {
            media_id,
            file_name: &file_name,
            content_type: processed.content_type,
            kind: Some(processed.kind),
            width: processed.width,
            height: processed.height,
            blurhash: processed.blurhash.as_deref(),
            small: small
                .as_ref()
                .map(|(name, width, height)| media::SmallStyle {
                    file_name: name,
                    width: *width,
                    height: *height,
                }),
            duration: processed.duration,
            frame_rate: processed.frame_rate.as_deref(),
            bitrate: processed.bitrate,
        },
    )
    .await
    .map_err(|e| AvCacheError::Transient(e.into()))?;
    media::set_file_sizes(
        &state.pool,
        media_id,
        processed.file_size,
        thumbnail_file_size,
    )
    .await
    .ok();
    Ok(())
}

/// Processes one claimed on-demand A/V job: reject-media blocks and permanent
/// failures resolve the row back to origin-URL-only service; transient
/// failures reschedule with backoff like the eager remote path.
async fn process_on_demand(state: &AppState, job: media::ClaimedJob) -> Result<(), ApiError> {
    if media_fetch_failure::is_abandoned(&state.pool, "attachment", job.media_id).await? {
        media::abandon_remote_download(&state.pool, job.media_id).await?;
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    }
    let Some(row) = media::find_by_ids(&state.pool, &[job.media_id])
        .await?
        .into_iter()
        .next()
        .filter(|m| m.processing == "queued")
    else {
        // Gone or already cached: drop the stale leased job.
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    };
    if plamenu_db::instance_policy::account_domain_rejects_media(&state.pool, row.account_id)
        .await?
    {
        media::abandon_remote_download(&state.pool, job.media_id).await?;
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    }
    match cache_remote_av(state, &row).await {
        Ok(()) => {
            note_proxy_fetch(&state.pool, "attachment", job.media_id, true).await;
            media::complete_job(&state.pool, job.id).await?;
            Ok(())
        }
        Err(AvCacheError::Permanent(reason)) => {
            tracing::warn!(
                media = job.media_id,
                reason,
                "remote video will not be cached; serving the origin url"
            );
            media::abandon_remote_download(&state.pool, job.media_id).await?;
            note_proxy_fetch(&state.pool, "attachment", job.media_id, false).await;
            media::complete_job(&state.pool, job.id).await?;
            Ok(())
        }
        Err(AvCacheError::Transient(error)) => {
            note_proxy_fetch(&state.pool, "attachment", job.media_id, false).await;
            let next = job.attempts + 1;
            if next < MAX_REMOTE_ATTEMPTS {
                // Re-queue with backoff; the leased row stays.
                media::reschedule_job(&state.pool, job.media_id, next, retry_delay(job.attempts))
                    .await?;
            } else {
                tracing::warn!(
                    media = job.media_id,
                    "giving up caching remote video; serving the origin url"
                );
                media::abandon_remote_download(&state.pool, job.media_id).await?;
                media::complete_job(&state.pool, job.id).await?;
            }
            Err(error)
        }
    }
}

/// Claims and processes due on-demand A/V jobs; returns how many were
/// claimed. One at a time — which also means at most one download hitting
/// any origin at any moment.
pub async fn run_due_on_demand(state: &AppState) -> u64 {
    let jobs = match media::claim_due_on_demand(&state.pool, 1).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim on-demand A/V jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        if let Err(error) = process_on_demand(state, job).await {
            tracing::warn!(error = %error.chain(), media = job.media_id, "on-demand A/V caching failed");
        }
    }
    claimed
}

/// Runs the on-demand A/V download lane until the process exits. Its own
/// task, deliberately separate from [`spawn`]: a multi-gigabyte video
/// download must never block avatars and previews behind it.
#[must_use]
pub fn spawn_av(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("on-demand A/V download worker started");
        loop {
            if run_due_on_demand(&state).await == 0
                && !crate::workers::pause(&state, IDLE_POLL).await
            {
                return;
            }
        }
    })
}

/// Maps a fetched image's `Content-Type` to a safe raster extension. Anything
/// outside this raster allowlist (notably `image/svg+xml`, which could carry
/// script when served inline) is refused — we never re-serve it, so the media
/// proxy fails closed for it.
fn safe_image_ext(content_type: &str) -> Option<&'static str> {
    match content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" | "image/apng" => Some("png"),
        "image/gif" => Some("gif"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        // Storable as arrived even though our still pipeline can't decode it.
        "image/avif" => Some("avif"),
        "image/jxl" => Some("jxl"),
        _ => None,
    }
}

/// The origin domain of an attachment (its author's account domain), for the
/// `reject_media` policy check; `None` for a local upload.
async fn attachment_domain(state: &AppState, item: &media::Media) -> Option<String> {
    account::find_by_id(&state.pool, item.account_id)
        .await
        .ok()
        .flatten()
        .and_then(|a| a.domain)
}

/// Ensures a remote attachment's bytes are cached locally, fetching and
/// re-processing on demand — the media proxy's cache-miss path (a
/// retention-evicted or never-downloaded remote attachment). Returns the
/// refreshed row (whose `file_name` is set once cached). A `reject_media`
/// domain is left uncached, like the eager path.
pub async fn ensure_attachment_cached(
    state: &AppState,
    media_id: i64,
) -> Result<Option<media::Media>, ApiError> {
    let Some(item) = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    if item.file_name.is_some() {
        return Ok(Some(item));
    }
    // On-demand A/V belongs to the play-triggered queue flow
    // ([`ensure_av_ready`]), never this inline path.
    if item.download_on_demand {
        return Ok(Some(item));
    }
    let Some(remote_url) = item.remote_url.clone() else {
        return Ok(Some(item));
    };
    if let Some(domain) = attachment_domain(state, &item).await
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, &domain).await?
    {
        return Ok(Some(item));
    }
    // Skip re-hitting an origin that recently failed (dead/blocked remote).
    if media_fetch_failure::is_cooling_down(
        &state.pool,
        "attachment",
        media_id,
        MEDIA_REFETCH_COOLDOWN,
    )
    .await?
    {
        return Ok(Some(item));
    }
    // Concurrent requests for the same uncached attachment coalesce into one
    // download: the first fetches, the rest wait for it and read its result.
    let Some(_guard) = InflightGuard::begin("attachment", media_id) else {
        while InflightGuard::running("attachment", media_id) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let refreshed = media::find_by_ids(&state.pool, &[media_id])
            .await?
            .into_iter()
            .next();
        return Ok(refreshed);
    };
    // A never-downloaded history image (and a retention-evicted image reached
    // directly through a still-valid proxy URL) is born `complete` with no
    // file. Enter the ordinary retryable cache lane before writing: the shared
    // completion primitive intentionally only finalizes `queued` rows. The
    // queued job is harmless if this inline attempt wins (the worker drops it
    // as stale), and preserves retry/backoff if this request fails or crashes.
    if item.processing == "complete" {
        media::enqueue_redownload(&state.pool, &[media_id]).await?;
    }
    // Best-effort: a failed download leaves the row uncached, and the caller
    // decides what to serve (404, or the origin when the viewer opted in).
    match cache_remote(state, media_id, &remote_url, &item.content_type).await {
        Ok(()) => {
            // The queue made this inline fetch crash-safe. It has done its job;
            // do not let the background worker download the same bytes again.
            media::complete_job_for_media(&state.pool, media_id)
                .await
                .ok();
        }
        Err(error) => {
            tracing::debug!(%media_id, %error, "on-demand attachment cache failed");
        }
    }
    let refreshed = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next();
    note_proxy_fetch(
        &state.pool,
        "attachment",
        media_id,
        refreshed.as_ref().is_some_and(|m| m.file_name.is_some()),
    )
    .await;
    Ok(refreshed)
}

/// The media proxy's attachment dispatch: images (and small eager A/V) cache
/// inline as before; an on-demand video's poster comes from its federated
/// thumbnail; the video itself is queued for the A/V lane and awaited
/// briefly ([`AV_WARMUP_WAIT`]).
pub async fn ensure_attachment_for_proxy(
    state: &AppState,
    media_id: i64,
    small: bool,
) -> Result<Option<media::Media>, ApiError> {
    let Some(item) = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    if !item.download_on_demand || item.file_name.is_some() {
        return ensure_attachment_cached(state, media_id).await;
    }
    if small {
        return ensure_attachment_poster(state, item).await;
    }
    ensure_av_ready(state, item).await
}

/// Caches the poster of a still-uncached on-demand video from its federated
/// `thumbnail_remote_url` — a timeline must be able to show the preview
/// without pulling a gigabyte video.
async fn ensure_attachment_poster(
    state: &AppState,
    item: media::Media,
) -> Result<Option<media::Media>, ApiError> {
    let media_id = item.id;
    if item.small_file_name.is_some() {
        return Ok(Some(item));
    }
    let Some(thumb_url) = item.thumbnail_remote_url.clone() else {
        return Ok(Some(item));
    };
    if let Some(domain) = attachment_domain(state, &item).await
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, &domain).await?
    {
        return Ok(Some(item));
    }
    let cooling = media_fetch_failure::is_cooling_down(
        &state.pool,
        "poster",
        media_id,
        MEDIA_REFETCH_COOLDOWN,
    )
    .await?;
    let Some(_guard) = InflightGuard::begin("poster", media_id) else {
        while InflightGuard::running("poster", media_id) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let refreshed = media::find_by_ids(&state.pool, &[media_id])
            .await?
            .into_iter()
            .next();
        return Ok(refreshed);
    };
    let result: Result<bool, ApiError> = async {
        let fetched = if cooling {
            // A cached failure may refer to an immutable preview URL the
            // producer has since rotated (or still advertises beside a working
            // thumbnail). Re-check the canonical object once rather than make
            // the reader wait out a backoff that no longer applies.
            let Some(fetched) = refreshed_attachment_poster(state, &item, &thumb_url).await else {
                return Ok(false);
            };
            fetched
        } else {
            match state.federation.fetch_media(&thumb_url).await {
                Ok(fetched) => fetched,
                Err(first_error) => {
                    // PeerTube can rotate preview files while transcoding. Its
                    // initial icon URL then 404s forever even though the current
                    // Video object advertises a healthy replacement or fallback.
                    let Some(fetched) = refreshed_attachment_poster(state, &item, &thumb_url).await
                    else {
                        return Err(ApiError::Internal(Box::new(first_error)));
                    };
                    fetched
                }
            }
        };
        let settings = state.settings_cache.get(&state.pool).await?;
        let params =
            media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
        let Some(processed) =
            media_processing::process_cached_image_blocking(fetched.bytes, POSTER_MAX_EDGE, params)
                .await?
        else {
            return Ok(false);
        };
        let small_name = format!(
            "{}.small.{}",
            media_processing::storage_stem(media_id),
            processed.extension
        );
        state
            .media
            .put(&small_name, processed.bytes)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        media::set_small_file(
            &state.pool,
            media_id,
            media::SmallStyle {
                file_name: &small_name,
                width: i32::try_from(processed.width).unwrap_or(i32::MAX),
                height: i32::try_from(processed.height).unwrap_or(i32::MAX),
            },
        )
        .await?;
        Ok(true)
    }
    .await;
    let cached = match result {
        Ok(cached) => cached,
        Err(error) => {
            tracing::debug!(%media_id, %error, "on-demand poster cache failed");
            false
        }
    };
    note_proxy_fetch(&state.pool, "poster", media_id, cached).await;
    let refreshed = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next();
    Ok(refreshed)
}

/// Re-fetches the attachment's canonical AP object after its stored poster
/// failed, persisting a newly-advertised poster URL for this and later views.
async fn refreshed_attachment_poster(
    state: &AppState,
    item: &media::Media,
    failed_url: &str,
) -> Option<FetchedMedia> {
    let status_id = item.status_id?;
    let status = plamenu_db::status::find_by_id(&state.pool, status_id)
        .await
        .ok()??;
    let uri = status.uri?;
    let object = state.federation.fetch_object(&uri).await.ok()?;
    for refreshed in crate::ingest::object_posters(&object)
        .into_iter()
        .filter(|url| *url != failed_url)
    {
        let Ok(fetched) = state.federation.fetch_media(refreshed).await else {
            continue;
        };
        let changed = media::update_remote_thumbnail(&state.pool, item.id, refreshed)
            .await
            .ok()?;
        if !changed {
            return None;
        }
        // A different immutable URL gets a fresh lifetime retry budget.
        let _ = media_fetch_failure::clear(&state.pool, "poster", item.id).await;
        return Some(fetched);
    }
    None
}

/// Queues an on-demand video for the A/V lane (the viewer pressed play) and
/// waits up to [`AV_WARMUP_WAIT`] for it to land, so short videos play on the
/// first tap. Returns the freshest row either way; the caller serves the
/// cached file, or falls back (origin URL with the viewer's opt-in, else 404)
/// while the download keeps running in the background.
async fn ensure_av_ready(
    state: &AppState,
    item: media::Media,
) -> Result<Option<media::Media>, ApiError> {
    let media_id = item.id;
    let settings = state.settings_cache.get(&state.pool).await?;
    if settings.remote_video_max_mb <= 0 {
        return Ok(Some(item));
    }
    if media_fetch_failure::is_cooling_down(
        &state.pool,
        "attachment",
        media_id,
        MEDIA_REFETCH_COOLDOWN,
    )
    .await?
    {
        return Ok(Some(item));
    }
    media::enqueue_on_demand_download(&state.pool, media_id).await?;
    let deadline = tokio::time::Instant::now() + AV_WARMUP_WAIT;
    let mut current = item;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Some(refreshed) = media::find_by_ids(&state.pool, &[media_id])
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        current = refreshed;
        // Cached — or resolved without a file (abandoned): stop waiting.
        if current.file_name.is_some() || current.processing == "complete" {
            break;
        }
    }
    Ok(Some(current))
}

/// Ensures an account's avatar/header is cached, fetching on demand. Returns
/// the refreshed account row.
pub async fn ensure_account_image_cached(
    state: &AppState,
    account_id: i64,
    which: &str,
) -> Result<Option<account::Account>, ApiError> {
    let Some(account) = account::find_by_id(&state.pool, account_id).await? else {
        return Ok(None);
    };
    let cached = if which == account_media::HEADER {
        &account.header_file_name
    } else {
        &account.avatar_file_name
    };
    if cached.is_some() {
        return Ok(Some(account));
    }
    // The actor carries no remote image: nothing to fetch, the serializer
    // hands out the placeholder. Not a fetch failure, so it is not recorded.
    let has_remote = if which == account_media::HEADER {
        account.header_remote_url.is_some()
    } else {
        account.avatar_remote_url.is_some()
    };
    if !has_remote {
        return Ok(Some(account));
    }
    // A `reject_media` domain block: serve the row uncached (the proxy 404s),
    // without a network attempt — and without a negative-cache entry that a
    // later unblock would have to wait out.
    if let Some(domain) = account.domain.as_deref()
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, domain).await?
    {
        return Ok(Some(account));
    }
    // Skip re-hitting an origin that recently failed (a rotated/deleted avatar
    // the remote now 404s, or a CDN blocking our IP).
    if media_fetch_failure::is_cooling_down(&state.pool, which, account_id, MEDIA_REFETCH_COOLDOWN)
        .await?
    {
        return Ok(Some(account));
    }
    // Best-effort: a failed download leaves the row uncached for the caller.
    if let Err(error) = cache_account_image(state, account_id, which).await {
        tracing::debug!(%account_id, which, %error, "on-demand account image cache failed");
    }
    let refreshed = account::find_by_id(&state.pool, account_id).await?;
    let now_cached = refreshed.as_ref().is_some_and(|a| {
        if which == account_media::HEADER {
            a.header_file_name.is_some()
        } else {
            a.avatar_file_name.is_some()
        }
    });
    note_proxy_fetch(&state.pool, which, account_id, now_cached).await;
    Ok(refreshed)
}

/// Fetches a remote image (an emoji or preview-card slot) and caches it. Under
/// the default `media_cached_image_processing` setting a still image is
/// downscaled to `max_edge` and re-encoded as AVIF — these copies serve local
/// clients only, never outbound federation. Everything else (the passthrough
/// setting, animated images, containers the still pipeline can't take) is
/// stored as arrived with a best-effort metadata strip, restricted to the
/// raster allowlist. Returns the stored file name, content type and size — or
/// `None` when an as-arrived type is not allowlisted.
async fn store_remote_image(
    state: &AppState,
    remote_url: &str,
    file_stem: &str,
    max_edge: u32,
) -> Result<Option<(String, String, i64)>, ApiError> {
    let fetched = state
        .federation
        .fetch_media(remote_url)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let mode = media_processing::CachedImage::from_setting(&settings.media_cached_image_processing);
    let params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    if mode == media_processing::CachedImage::Avif
        && let Some(processed) =
            media_processing::process_cached_image_blocking(fetched.bytes.clone(), max_edge, params)
                .await?
    {
        let file_name = format!("{file_stem}.{}", processed.extension);
        let file_size = i64::try_from(processed.bytes.len()).unwrap_or(i64::MAX);
        state
            .media
            .put(&file_name, processed.bytes)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        return Ok(Some((
            file_name,
            processed.content_type.to_owned(),
            file_size,
        )));
    }
    let Some(ext) = safe_image_ext(&fetched.content_type) else {
        return Ok(None);
    };
    let content_type = media_processing::content_type_for(&format!("x.{ext}")).to_owned();
    let file_name = format!("{file_stem}.{ext}");
    let bytes = media_processing::strip_cached_metadata(fetched.bytes);
    let file_size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok(Some((file_name, content_type, file_size)))
}

/// Ensures a remote custom emoji's image is cached, fetching on demand — the
/// media proxy's cache-miss path (remote emoji are never downloaded eagerly).
pub async fn ensure_emoji_cached(
    state: &AppState,
    emoji_id: i64,
) -> Result<Option<plamenu_db::custom_emoji::CustomEmoji>, ApiError> {
    use plamenu_db::custom_emoji;
    let Some(emoji) = custom_emoji::find_by_id(&state.pool, emoji_id).await? else {
        return Ok(None);
    };
    if emoji.image_file_name.is_some() {
        return Ok(Some(emoji));
    }
    let Some(remote_url) = emoji.image_remote_url.clone() else {
        return Ok(Some(emoji));
    };
    if let Some(domain) = emoji.domain.as_deref()
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, domain).await?
    {
        return Ok(Some(emoji));
    }
    if media_fetch_failure::is_cooling_down(&state.pool, "emoji", emoji_id, MEDIA_REFETCH_COOLDOWN)
        .await?
    {
        return Ok(Some(emoji));
    }
    match store_remote_image(
        state,
        &remote_url,
        &format!("{emoji_id}.emoji"),
        media_processing::EMOJI_MAX_EDGE,
    )
    .await
    {
        Ok(Some((file_name, content_type, file_size))) => {
            custom_emoji::set_image_file(
                &state.pool,
                emoji_id,
                &file_name,
                &content_type,
                file_size,
            )
            .await?;
        }
        Ok(None) => {}
        Err(error) => tracing::debug!(%emoji_id, %error, "on-demand emoji cache failed"),
    }
    let refreshed = custom_emoji::find_by_id(&state.pool, emoji_id).await?;
    note_proxy_fetch(
        &state.pool,
        "emoji",
        emoji_id,
        refreshed
            .as_ref()
            .is_some_and(|e| e.image_file_name.is_some()),
    )
    .await;
    Ok(refreshed)
}

/// Ensures a preview card's image is cached, fetching on demand — the media
/// proxy's cache-miss path (card images are never downloaded eagerly).
pub async fn ensure_card_cached(
    state: &AppState,
    card_id: i64,
) -> Result<Option<plamenu_db::preview_card::PreviewCard>, ApiError> {
    use plamenu_db::preview_card;
    let Some(card) = preview_card::find_by_id(&state.pool, card_id).await? else {
        return Ok(None);
    };
    if card.image_file_name.is_some() {
        return Ok(Some(card));
    }
    let Some(image_url) = card.image_url.clone() else {
        return Ok(Some(card));
    };
    if let Some(domain) = crate::link_preview::url_host(&image_url)
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, &domain).await?
    {
        return Ok(Some(card));
    }
    if media_fetch_failure::is_cooling_down(&state.pool, "card", card_id, MEDIA_REFETCH_COOLDOWN)
        .await?
    {
        return Ok(Some(card));
    }
    match store_remote_image(
        state,
        &image_url,
        &format!("{card_id}.card"),
        media_processing::CARD_MAX_EDGE,
    )
    .await
    {
        Ok(Some((file_name, content_type, file_size))) => {
            preview_card::set_image_file(
                &state.pool,
                card_id,
                &file_name,
                &content_type,
                file_size,
            )
            .await?;
        }
        Ok(None) => {}
        Err(error) => tracing::debug!(%card_id, %error, "on-demand card image cache failed"),
    }
    let refreshed = preview_card::find_by_id(&state.pool, card_id).await?;
    note_proxy_fetch(
        &state.pool,
        "card",
        card_id,
        refreshed
            .as_ref()
            .is_some_and(|c| c.image_file_name.is_some()),
    )
    .await;
    Ok(refreshed)
}

/// Transcodes a spooled local (v2) upload and completes — or permanently fails
/// — its row.
async fn transcode_local(state: &AppState, media_id: i64, orig_name: &str) -> Result<(), ApiError> {
    let result: Result<(), ApiError> = async {
        let original = state
            .media
            .get(orig_name)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        let settings = state.settings_cache.get(&state.pool).await?;
        let params = crate::media_transcode::TranscodeParams::from_settings(&settings);
        let processed =
            crate::media_transcode::process_av(&state.config, &original, &params).await?;
        let stored = store_av_output(state, media_id, &processed).await?;
        media::complete_processing(
            &state.pool,
            media::ProcessedMedia {
                media_id,
                file_name: &stored.file_name,
                content_type: processed.content_type,
                kind: Some(processed.kind),
                width: processed.width,
                height: processed.height,
                blurhash: processed.blurhash.as_deref(),
                small: stored
                    .small
                    .as_ref()
                    .map(|(name, width, height)| media::SmallStyle {
                        file_name: name,
                        width: *width,
                        height: *height,
                    }),
                duration: processed.duration,
                frame_rate: processed.frame_rate.as_deref(),
                bitrate: processed.bitrate,
            },
        )
        .await?;
        media::set_file_sizes(
            &state.pool,
            media_id,
            stored.file_size,
            stored.thumbnail_file_size,
        )
        .await
        .ok();
        Ok(())
    }
    .await;

    if let Err(error) = result {
        media::fail_processing(&state.pool, media_id).await?;
        let _ = state.media.delete(orig_name).await;
        return Err(error);
    }
    // The spooled original is no longer needed.
    let _ = state.media.delete(orig_name).await;
    Ok(())
}

/// Processes one claimed job: a remote attachment is downloaded + cached (with
/// backoff on failure), a local upload is transcoded.
async fn process_claimed(state: &AppState, job: media::ClaimedJob) -> Result<(), ApiError> {
    let Some(row) = media::find_by_ids(&state.pool, &[job.media_id])
        .await?
        .into_iter()
        .next()
        .filter(|m| m.processing == "queued")
    else {
        // The attachment is gone or already processed: the leased job is stale,
        // so drop it.
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    };

    // Remote rows always carry a remote_url; local uploads never do.
    if let Some(remote_url) = &row.remote_url {
        if media_fetch_failure::is_abandoned(&state.pool, "attachment", job.media_id).await? {
            media::abandon_remote_download(&state.pool, job.media_id).await?;
            media::complete_job(&state.pool, job.id).await?;
            return Ok(());
        }
        // A `reject_media` domain block that landed after the job was queued:
        // drop the download, the row completes serving only its origin URL.
        if plamenu_db::instance_policy::account_domain_rejects_media(&state.pool, row.account_id)
            .await?
        {
            media::abandon_remote_download(&state.pool, job.media_id).await?;
            media::complete_job(&state.pool, job.id).await?;
            return Ok(());
        }
        if let Err(error) = cache_remote(state, job.media_id, remote_url, &row.content_type).await {
            note_proxy_fetch(&state.pool, "attachment", job.media_id, false).await;
            let next = job.attempts + 1;
            if next < MAX_REMOTE_ATTEMPTS {
                // Re-queue with backoff; the leased row stays and becomes due
                // again after the delay.
                media::reschedule_job(&state.pool, job.media_id, next, retry_delay(job.attempts))
                    .await?;
            } else {
                tracing::warn!(
                    media = job.media_id,
                    "giving up caching remote media; serving the origin url"
                );
                // Without this the row stays `queued` forever with no job
                // left to run it — resolve it like the reject_media path.
                media::abandon_remote_download(&state.pool, job.media_id).await?;
                media::complete_job(&state.pool, job.id).await?;
            }
            return Err(error);
        }
        note_proxy_fetch(&state.pool, "attachment", job.media_id, true).await;
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    }

    let Some(orig_name) = row.file_name.clone() else {
        media::complete_job(&state.pool, job.id).await?;
        return Ok(());
    };
    // A local transcode is single-attempt: complete the job whether it
    // succeeds or fails permanently, so it is never left leased.
    let result = transcode_local(state, job.media_id, &orig_name).await;
    media::complete_job(&state.pool, job.id).await?;
    result
}

/// Downloads an actor's avatar or header and caches it (Mastodon's avatar
/// 400px / header 1500px bounds; no preview style or blurhash), then records
/// the cached file name and drops the previous file if its extension changed.
/// Unlike a *local* avatar/header (always a Mastodon-compatible photo — other
/// servers fetch those), the cached copy of a remote one serves local clients
/// only, so under the default `media_cached_image_processing` setting a still
/// image re-encodes to AVIF; animated ones are kept as arrived instead of
/// being frozen to their first frame by the photo re-encode.
async fn cache_account_image(
    state: &AppState,
    account_id: i64,
    which: &str,
) -> Result<(), ApiError> {
    let account = account::find_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // A `reject_media` domain block that landed after the job was queued:
    // the entity keeps serving the remote URL, like an uncached image.
    if let Some(domain) = account.domain.as_deref()
        && plamenu_db::instance_policy::domain_rejects_media(&state.pool, domain).await?
    {
        return Ok(());
    }
    let (remote_url, max_edge, old_file) = if which == account_media::HEADER {
        (
            account.header_remote_url,
            media_processing::HEADER_MAX_EDGE,
            account.header_file_name,
        )
    } else {
        (
            account.avatar_remote_url,
            media_processing::AVATAR_MAX_EDGE,
            account.avatar_file_name,
        )
    };
    // The actor dropped the image since the job was queued: nothing to fetch.
    let Some(remote_url) = remote_url else {
        return Ok(());
    };

    let fetched = state
        .federation
        .fetch_media(&remote_url)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let settings = state.settings_cache.get(&state.pool).await?;
    let mode = media_processing::CachedImage::from_setting(&settings.media_cached_image_processing);
    let params =
        media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let recoded = if mode == media_processing::CachedImage::Avif {
        media_processing::process_cached_image_blocking(fetched.bytes.clone(), max_edge, params)
            .await?
    } else {
        None
    };
    let (bytes, extension) = if let Some(processed) = recoded {
        (processed.bytes, processed.extension)
    } else if let Some((_, extension)) = media_processing::sniffed_raw_format(&fetched.bytes) {
        // Kept as arrived: the passthrough setting, an animation the still
        // pipeline would freeze, or a recognized image whose optional
        // re-encode failed.
        (
            media_processing::strip_cached_metadata(fetched.bytes),
            extension,
        )
    } else {
        // A container the sniffer doesn't recognize: the legacy photo
        // re-encode (which also rejects non-images, like before).
        let settings = state.settings_cache.get(&state.pool).await?;
        let params =
            media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
        let processed =
            media_processing::process_image_blocking(fetched.bytes, max_edge, params).await?;
        (processed.bytes, processed.extension)
    };
    let file_name = format!("{account_id}.{which}.{extension}");
    state
        .media
        .put(&file_name, bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    account_media::set_file_name(&state.pool, account_id, which, &file_name).await?;
    if let Some(old) = old_file
        && old != file_name
    {
        let _ = state.media.delete(&old).await;
    }
    Ok(())
}

/// Processes one avatar/header job, rescheduling with backoff on failure (the
/// origin may be briefly unreachable), then giving up — the entity keeps
/// serving the remote URL until a later refresh re-queues it.
async fn process_account_media(
    state: &AppState,
    job: &account_media::AccountMediaJob,
) -> Result<(), ApiError> {
    if media_fetch_failure::is_abandoned(&state.pool, &job.which, job.account_id).await? {
        account_media::complete(&state.pool, job.id).await?;
        return Ok(());
    }
    if let Err(error) = cache_account_image(state, job.account_id, &job.which).await {
        note_proxy_fetch(&state.pool, &job.which, job.account_id, false).await;
        let next = job.attempts + 1;
        if next < MAX_REMOTE_ATTEMPTS {
            // Re-queue with backoff; the leased row stays.
            account_media::reschedule(
                &state.pool,
                job.account_id,
                &job.which,
                next,
                retry_delay(job.attempts),
            )
            .await?;
        } else {
            // Retries exhausted: drop the leased job so it is not reclaimed.
            account_media::complete(&state.pool, job.id).await?;
        }
        return Err(error);
    }
    note_proxy_fetch(&state.pool, &job.which, job.account_id, true).await;
    account_media::complete(&state.pool, job.id).await?;
    Ok(())
}

/// Claims and processes due avatar/header download jobs; returns how many were
/// claimed.
pub async fn run_due_account_media(state: &AppState) -> u64 {
    let jobs = match account_media::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim account media jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        if let Err(error) = process_account_media(state, &job).await {
            tracing::warn!(error = %error.chain(), account = job.account_id, which = %job.which, "account media processing failed");
        }
    }
    claimed
}

/// Claims and processes due media jobs; returns how many were claimed.
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match media::claim_due_processing(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim media processing jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        if let Err(error) = process_claimed(state, job).await {
            tracing::warn!(error = %error.chain(), media = job.media_id, "media processing failed");
        }
    }
    claimed
}

/// Deletes the files a sweep freed (original + small) from the media store.
async fn delete_cleared(state: &AppState, files: &[media::ClearedFile]) {
    for entry in files {
        for name in [entry.file_name.as_deref(), entry.small_file_name.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = state.media.delete(name).await;
        }
    }
}

/// One retention pass over every cache class (M39): time-based eviction per
/// grade (attachments, videos + HLS, profile images, cards, emoji), then the
/// size caps, then the orphan vacuum. Evicted copies are redownloaded on
/// demand; local uploads are never touched (bar the orphan vacuum).
#[allow(
    clippy::too_many_lines,
    reason = "one linear sweep over the retention classes"
)]
pub async fn run_retention(state: &AppState) {
    /// Rows handled per class per pass; the next pass picks up any remainder.
    const SWEEP_LIMIT: i64 = 500;
    /// Mastodon's orphan-vacuum TTL for unattached uploads.
    const ORPHAN_TTL: Duration = Duration::from_hours(24);
    /// Ceiling on size-cap eviction batches per pass — a pathological
    /// backlog is worked off over successive hourly passes instead of one
    /// unbounded loop.
    const MAX_SIZE_BATCHES: u32 = 20;

    // A transient settings read failure skips this pass's eviction rather
    // than surprising the operator with stale fallback behavior.
    let settings = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "settings read failed; skipping media retention pass");
            return;
        }
    };
    let attachment_days = u64::try_from(settings.media_cache_retention_days).unwrap_or(0);
    // Videos follow the attachment period unless given their own.
    let video_days = settings
        .media_video_retention_days
        .map_or(attachment_days, |days| u64::try_from(days).unwrap_or(0));

    if attachment_days > 0 {
        let retention = Duration::from_secs(attachment_days.saturating_mul(86_400));
        match media::evict_cached_remote(&state.pool, retention, SWEEP_LIMIT).await {
            Ok(cleared) => delete_cleared(state, &cleared).await,
            Err(error) => tracing::error!(%error, "remote media eviction failed"),
        }
    }
    if video_days > 0 {
        let retention = Duration::from_secs(video_days.saturating_mul(86_400));
        match media::evict_cached_remote_video(&state.pool, retention, SWEEP_LIMIT).await {
            Ok(cleared) => delete_cleared(state, &cleared).await,
            Err(error) => tracing::error!(%error, "remote video eviction failed"),
        }
        // Cached HLS segments are their own store files, keyed per byte
        // range; evict the cold ones on the video retention window.
        let retention_secs = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
        match media::hls_segments_evict(&state.pool, retention_secs).await {
            Ok(files) => delete_files(state, &files).await,
            Err(error) => tracing::error!(%error, "hls segment eviction failed"),
        }
    }
    if settings.media_profile_retention_days > 0 {
        let retention = days_duration(settings.media_profile_retention_days);
        match account_media::evict_cached_images(&state.pool, retention, SWEEP_LIMIT).await {
            Ok(files) => delete_files(state, &files).await,
            Err(error) => tracing::error!(%error, "profile image eviction failed"),
        }
    }
    if settings.media_card_retention_days > 0 {
        let retention = days_duration(settings.media_card_retention_days);
        match plamenu_db::preview_card::evict_cached_images(&state.pool, retention, SWEEP_LIMIT)
            .await
        {
            Ok(files) => delete_files(state, &files).await,
            Err(error) => tracing::error!(%error, "card image eviction failed"),
        }
    }
    if settings.media_emoji_retention_days > 0 {
        let retention = days_duration(settings.media_emoji_retention_days);
        match plamenu_db::custom_emoji::evict_cached_images(&state.pool, retention, SWEEP_LIMIT)
            .await
        {
            Ok(files) => delete_files(state, &files).await,
            Err(error) => tracing::error!(%error, "emoji image eviction failed"),
        }
    }

    // Size caps: oldest-first (attachments) / least-recently-watched (video)
    // until the class fits, bounded per pass.
    if settings.media_cache_max_gb > 0 {
        let cap = gib_bytes(settings.media_cache_max_gb);
        for _ in 0..MAX_SIZE_BATCHES {
            match media::cached_remote_bytes(&state.pool, false).await {
                Ok(total) if total <= cap => break,
                Ok(_) => {
                    match media::evict_cached_remote_oldest(&state.pool, false, SWEEP_LIMIT).await {
                        Ok(cleared) if cleared.is_empty() => break,
                        Ok(cleared) => delete_cleared(state, &cleared).await,
                        Err(error) => {
                            tracing::error!(%error, "attachment size-cap eviction failed");
                            break;
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "attachment cache size read failed");
                    break;
                }
            }
        }
    }
    if settings.media_video_cache_max_gb > 0 {
        let cap = gib_bytes(settings.media_video_cache_max_gb);
        for _ in 0..MAX_SIZE_BATCHES {
            let video = media::cached_remote_bytes(&state.pool, true).await;
            let hls = media::hls_segments_bytes(&state.pool).await;
            match (video, hls) {
                (Ok(video), Ok(hls)) if video.saturating_add(hls) <= cap => break,
                (Ok(video), Ok(hls)) => {
                    // Whichever sub-class holds more bytes yields first.
                    let evicted = if hls >= video {
                        match media::hls_segments_evict_lru(&state.pool, SWEEP_LIMIT).await {
                            Ok(files) => {
                                let count = files.len();
                                delete_files(state, &files).await;
                                count
                            }
                            Err(error) => {
                                tracing::error!(%error, "hls size-cap eviction failed");
                                break;
                            }
                        }
                    } else {
                        match media::evict_cached_remote_oldest(&state.pool, true, SWEEP_LIMIT)
                            .await
                        {
                            Ok(cleared) => {
                                let count = cleared.len();
                                delete_cleared(state, &cleared).await;
                                count
                            }
                            Err(error) => {
                                tracing::error!(%error, "video size-cap eviction failed");
                                break;
                            }
                        }
                    };
                    if evicted == 0 {
                        break;
                    }
                }
                (Err(error), _) | (_, Err(error)) => {
                    tracing::error!(%error, "video cache size read failed");
                    break;
                }
            }
        }
    }

    match media::vacuum_orphans(&state.pool, ORPHAN_TTL, SWEEP_LIMIT).await {
        Ok(removed) => delete_cleared(state, &removed).await,
        Err(error) => tracing::error!(%error, "orphan media vacuum failed"),
    }
}

/// A whole-day retention period from an admin day-count setting.
fn days_duration(days: i32) -> Duration {
    Duration::from_secs(u64::try_from(days).unwrap_or(0).saturating_mul(86_400))
}

/// A GiB cap setting as bytes.
fn gib_bytes(gib: i32) -> i64 {
    i64::from(gib.max(0)).saturating_mul(1024 * 1024 * 1024)
}

/// Deletes a list of freed store files (best-effort).
async fn delete_files(state: &AppState, files: &[String]) {
    for name in files {
        let _ = state.media.delete(name).await;
    }
}

/// Purges every cached file from `domain` — status attachments, account
/// avatars/headers and custom emoji (Mastodon's `DomainClearMediaWorker` /
/// `ClearDomainMediaService`). Batched; loops until nothing is left.
/// Rows keep their remote URLs, but with the block's `reject_media` in force
/// (M31) nothing re-downloads them.
pub async fn purge_domain_media(state: &AppState, domain: &str) {
    const BATCH: i64 = 500;
    loop {
        match media::evict_cached_for_domain(&state.pool, domain, BATCH).await {
            Ok(cleared) if cleared.is_empty() => break,
            Ok(cleared) => delete_cleared(state, &cleared).await,
            Err(error) => {
                tracing::error!(%error, domain, "domain media purge failed");
                return;
            }
        }
    }
    loop {
        match account_media::clear_cached_for_domain(&state.pool, domain, BATCH).await {
            Ok(cleared) if cleared.is_empty() => break,
            Ok(cleared) => delete_cleared(state, &cleared).await,
            Err(error) => {
                tracing::error!(%error, domain, "domain profile-image purge failed");
                return;
            }
        }
    }
    match plamenu_db::custom_emoji::purge_domain(&state.pool, domain).await {
        Ok(files) => {
            for name in &files {
                let _ = state.media.delete(name).await;
            }
        }
        Err(error) => {
            tracing::error!(%error, domain, "domain emoji purge failed");
            return;
        }
    }
    tracing::info!(domain, "cached media purged for blocked domain");
}

/// Fires [`purge_domain_media`] in the background when a created or updated
/// domain block calls for it: `reject_media`, or a full suspension (whose
/// visibility gates already hide the domain — the cache is dead weight).
pub fn spawn_domain_purge(state: &AppState, block: &plamenu_db::instance_policy::DomainBlock) {
    if !(block.reject_media || block.severity == "suspend") {
        return;
    }
    let state = state.clone();
    let domain = block.domain.clone();
    tokio::spawn(async move {
        purge_domain_media(&state, &domain).await;
    });
}

/// Runs the retention sweep hourly until the process exits.
#[must_use]
pub fn spawn_retention(state: AppState) -> JoinHandle<()> {
    const SWEEP_INTERVAL: Duration = Duration::from_hours(1);
    tokio::spawn(async move {
        loop {
            run_retention(&state).await;
            if !crate::workers::pause(&state, SWEEP_INTERVAL).await {
                return;
            }
        }
    })
}

/// Runs the transcoding loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("media processing worker started");
        loop {
            let attachments = run_due(&state).await;
            let profile_images = run_due_account_media(&state).await;
            if attachments == 0
                && profile_images == 0
                && !crate::workers::pause(&state, IDLE_POLL).await
            {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn passive_files_keep_safe_types_and_neutralize_active_ones() {
        assert_eq!(
            passive_file_format("text/markdown"),
            ("text/markdown", "md")
        );
        assert_eq!(
            passive_file_format("application/pdf"),
            ("application/pdf", "pdf")
        );
        assert_eq!(
            passive_file_format("text/html; charset=utf-8"),
            ("application/octet-stream", "bin")
        );
        assert_eq!(
            passive_file_format("image/svg+xml"),
            ("application/octet-stream", "bin")
        );
    }

    /// Single-flight guard lifecycle, in one test because a deadlocked
    /// `INFLIGHT` poisons every later test in the process: dropping the
    /// winner's guard must clear the mark, and the losing side of a race
    /// must get [`None`] back without touching the `INFLIGHT` lock from
    /// its own destructor — an eagerly-built guard dropped inside `begin`
    /// re-locks the mutex the enclosing expression still holds and freezes
    /// every runtime worker (staging outage 2026-07-12). The contended
    /// call therefore runs last, on a helper thread, so a regression fails
    /// this test instead of hanging the suite.
    #[test]
    fn guard_clears_on_drop_and_contended_begin_coalesces() {
        let guard = InflightGuard::begin("test-inflight", 1).expect("uncontended begin");
        assert!(InflightGuard::running("test-inflight", 1));
        drop(guard);
        assert!(!InflightGuard::running("test-inflight", 1));

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let held = InflightGuard::begin("test-inflight", 2);
            let contended = InflightGuard::begin("test-inflight", 2);
            tx.send(held.is_some() && contended.is_none()).unwrap();
            drop(held);
        });
        let outcome = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("InflightGuard::begin deadlocked on the contended path");
        assert!(outcome, "first begin must win, second must coalesce");
    }
}
