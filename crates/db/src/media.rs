//! Media attachments: local uploads and remote references.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(clippy::struct_excessive_bools)] // direct row shape; PostgreSQL stores these independent flags
pub struct Media {
    pub id: i64,
    pub account_id: i64,
    pub status_id: Option<i64>,
    pub file_name: Option<String>,
    pub remote_url: Option<String>,
    pub content_type: String,
    pub description: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub created_at: OffsetDateTime,
    /// Mastodon's attachment type for local uploads (`image`, `gifv`,
    /// `video`, `audio`); `None` for remote rows (derived from
    /// `content_type` instead).
    pub kind: Option<String>,
    pub blurhash: Option<String>,
    pub focus_x: Option<f64>,
    pub focus_y: Option<f64>,
    pub small_file_name: Option<String>,
    pub small_width: Option<i32>,
    pub small_height: Option<i32>,
    pub thumbnail_remote_url: Option<String>,
    /// Seconds (video/audio).
    pub duration: Option<f64>,
    /// The raw `avg_frame_rate` fraction, e.g. `"30/1"` (Mastodon stores
    /// the Ruby `Rational` the same way).
    pub frame_rate: Option<String>,
    /// Bits per second (video/audio).
    pub bitrate: Option<i64>,
    /// `queued` | `complete` | `failed`.
    pub processing: String,
    /// `PeerTube` separated-audio HLS: the companion audio-only stream muxed
    /// into the cached copy (the video rendition alone is silent).
    pub remote_audio_url: Option<String>,
    /// Remote A/V that downloads only on the first play request, never
    /// eagerly at ingest or on status render (long-form video).
    pub download_on_demand: bool,
    /// `PeerTube` live broadcast state — `waiting` (announced, not started),
    /// `live` (on air), `ended`. `None` for everything that is not a live.
    /// Carried on the row rather than side-loaded like the rendition ladder
    /// because every lane needs it: the player, the HLS proxy's cache policy,
    /// the progressive gateway, the state-refresh path and the serializer.
    pub live_state: Option<String>,
    /// A permanent live re-arms for the next broadcast instead of ending, so
    /// its `waiting` means "between streams", not "never started".
    pub live_permanent: bool,
    /// HLS-native remote video (`PeerTube`): the origin master playlist URL.
    /// `None` for plain-mp4 media, and for a live that has not started yet.
    pub hls_master_url: Option<String>,
    /// Plain-text transcript (video transcripts should also describe important
    /// visual information). Local authoring only; federated support has no
    /// interoperable `ActivityPub` field yet.
    pub transcript: Option<String>,
    /// Author-supplied `WebVTT` captions, served from a same-origin media route.
    pub caption_vtt: Option<String>,
    /// The author intentionally marked an image as adding no information.
    pub decorative: bool,
    /// The author confirms that an integrated audio description is present.
    pub audio_described: bool,
    /// The ordinary soundtrack already conveys all important visual
    /// information, so no additional audio description is necessary.
    pub visuals_conveyed_in_audio: bool,
}

/// Author-supplied accessibility intent stored alongside one attachment.
#[derive(Debug, Clone, Copy, Default)]
pub struct AccessibilityUpdate<'a> {
    pub transcript: Option<&'a str>,
    /// Outer `None` keeps existing captions; `Some(None)` clears them.
    pub caption_vtt: Option<Option<&'a str>>,
    pub decorative: bool,
    pub audio_described: bool,
    pub visuals_conveyed_in_audio: bool,
}

impl Media {
    /// Mastodon's `not_processed?`: still queued (or failed) — such media
    /// serve no `url` and cannot be attached to statuses.
    #[must_use]
    pub fn not_processed(&self) -> bool {
        self.processing != "complete"
    }

    /// The Mastodon attachment type: the stored kind, or derived from the
    /// content type for rows without one (remote references).
    #[must_use]
    pub fn kind_or_derived(&self) -> &str {
        if let Some(kind) = &self.kind {
            return kind;
        }
        if self.content_type.starts_with("image/") {
            "image"
        } else if self.content_type.starts_with("video/") {
            "video"
        } else if self.content_type.starts_with("audio/") {
            "audio"
        } else if self.live_state.is_some() {
            // A live broadcast's only declared media type is its HLS playlist
            // (`application/x-mpegURL`), which no `*/`-prefix rule covers —
            // but it is a video to every client that will render it.
            "video"
        } else {
            "unknown"
        }
    }

    /// Mastodon's `audio_or_video?` — notably excluding `gifv`, which may
    /// mix with images in a post.
    #[must_use]
    pub fn audio_or_video(&self) -> bool {
        matches!(self.kind_or_derived(), "audio" | "video")
    }
}

/// Whether an immutable storage key may be served publicly. Suspension makes
/// profile images, attachment originals/previews and cached HLS segments
/// private immediately, without deleting bytes; unsuspension therefore
/// republishes them automatically.
pub async fn public_file_allowed(pool: &PgPool, file_name: &str) -> Result<bool, DbError> {
    let allowed = sqlx::query_scalar!(
        r#"
        SELECT NOT EXISTS (
            SELECT 1 FROM accounts a
            WHERE a.suspended_at IS NOT NULL
              AND (a.avatar_file_name = $1 OR a.header_file_name = $1)
            UNION ALL
            SELECT 1 FROM media_attachments m
            JOIN accounts a ON a.id = m.account_id
            WHERE a.suspended_at IS NOT NULL
              AND (m.file_name = $1 OR m.small_file_name = $1)
            UNION ALL
            SELECT 1 FROM media_hls_segments seg
            JOIN media_attachments m ON m.id = seg.media_id
            JOIN accounts a ON a.id = m.account_id
            WHERE a.suspended_at IS NOT NULL AND seg.cache_file = $1
        ) AS "allowed!"
        "#,
        file_name,
    )
    .fetch_one(pool)
    .await?;
    Ok(allowed)
}

/// Fast ownership gate for media-id routes (proxy, HLS, progressive/live).
pub async fn owner_suspended(pool: &PgPool, media_id: i64) -> Result<bool, DbError> {
    let suspended = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM media_attachments m
            JOIN accounts a ON a.id = m.account_id
            WHERE m.id = $1 AND a.suspended_at IS NOT NULL
        ) AS "suspended!"
        "#,
        media_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(suspended)
}

/// The downscaled preview style of a processed upload.
#[derive(Debug, Clone, Copy)]
pub struct SmallStyle<'a> {
    pub file_name: &'a str,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
pub struct NewLocalMedia<'a> {
    pub account_id: i64,
    /// Pre-generated row id (the storage file name embeds it).
    pub media_id: i64,
    pub file_name: &'a str,
    pub content_type: &'a str,
    pub description: Option<&'a str>,
    pub kind: &'a str,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub blurhash: Option<&'a str>,
    pub focus: Option<(f64, f64)>,
    pub small: Option<SmallStyle<'a>>,
    pub duration: Option<f64>,
    pub frame_rate: Option<&'a str>,
    pub bitrate: Option<i64>,
}

impl<'a> NewLocalMedia<'a> {
    /// A minimal already-processed image row; callers fill in the rest.
    #[must_use]
    pub fn new(account_id: i64, media_id: i64, file_name: &'a str, content_type: &'a str) -> Self {
        Self {
            account_id,
            media_id,
            file_name,
            content_type,
            description: None,
            kind: "image",
            width: None,
            height: None,
            blurhash: None,
            focus: None,
            small: None,
            duration: None,
            frame_rate: None,
            bitrate: None,
        }
    }
}

pub async fn create_local(pool: &PgPool, new: NewLocalMedia<'_>) -> Result<Media, DbError> {
    let (focus_x, focus_y) = match new.focus {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };
    let media = sqlx::query_as!(
        Media,
        r#"
        INSERT INTO media_attachments (id, account_id, file_name, content_type,
                                       description, kind, width, height, blurhash,
                                       focus_x, focus_y, small_file_name,
                                       small_width, small_height, duration,
                                       frame_rate, bitrate, processing)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, 'complete')
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        new.media_id,
        new.account_id,
        new.file_name,
        new.content_type,
        new.description,
        new.kind,
        new.width,
        new.height,
        new.blurhash,
        focus_x,
        focus_y,
        new.small.map(|s| s.file_name),
        new.small.map(|s| s.width),
        new.small.map(|s| s.height),
        new.duration,
        new.frame_rate,
        new.bitrate,
    )
    .fetch_one(pool)
    .await?;
    Ok(media)
}

#[derive(Debug)]
pub struct NewQueuedMedia<'a> {
    pub account_id: i64,
    pub media_id: i64,
    /// The spooled original (`{id}.orig`) the worker will transcode.
    pub file_name: &'a str,
    /// The detected input type; processing replaces it with the output's.
    pub content_type: &'a str,
    pub kind: &'a str,
    pub description: Option<&'a str>,
    pub focus: Option<(f64, f64)>,
}

/// Creates a queued attachment plus its transcoding job (one transaction —
/// the worker must never see one without the other).
pub async fn create_queued(pool: &PgPool, new: NewQueuedMedia<'_>) -> Result<Media, DbError> {
    let (focus_x, focus_y) = match new.focus {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };
    let mut tx = pool.begin().await?;
    let media = sqlx::query_as!(
        Media,
        r#"
        INSERT INTO media_attachments (id, account_id, file_name, content_type,
                                       description, kind, focus_x, focus_y,
                                       processing)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'queued')
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        new.media_id,
        new.account_id,
        new.file_name,
        new.content_type,
        new.description,
        new.kind,
        focus_x,
        focus_y,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO media_processing_jobs (id, media_id) VALUES ($1, $2)",
        id::next(),
        new.media_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(media)
}

/// How long a claimed processing job stays invisible before a crashed worker's
/// job becomes due again. Transcodes and remote-image downloads
/// are quick, so ten minutes comfortably exceeds one attempt.
const PROCESSING_LEASE_SECONDS: f64 = 600.0;

/// A longer lease for the on-demand A/V lane: a multi-gigabyte video download
/// can legitimately run for many minutes, and must not be reclaimed mid-flight.
const ON_DEMAND_LEASE_SECONDS: f64 = 1800.0;

/// A claimed media job: its own id (to [`complete_job`] it), which attachment,
/// and how many prior attempts it has had (0 on first run; bumped each time a
/// remote download is rescheduled).
#[derive(Debug, Clone, Copy)]
pub struct ClaimedJob {
    pub id: i64,
    pub media_id: i64,
    pub attempts: i32,
}

/// Claims up to `limit` due jobs — except on-demand A/V downloads, which belong
/// to their own worker lane (a multi-gigabyte video download must never block
/// avatars and previews behind it). Each is leased for
/// [`PROCESSING_LEASE_SECONDS`] rather than deleted: the row
/// survives until the worker [`complete_job`]s it, so a crash between claim and
/// completion lets the lease expire and the attachment is processed again
/// instead of being stranded `queued` with no job. A local transcode fails
/// permanently on its single attempt; a remote download reschedules itself with
/// backoff (see [`reschedule_job`]), carrying `attempts` forward. `attempts` is
/// owned by [`reschedule_job`], not the claim, so a crash reclaim does not
/// consume the retry budget.
pub async fn claim_due_processing(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedJob>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedJob,
        r#"
        UPDATE media_processing_jobs SET run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT j.id FROM media_processing_jobs j
            JOIN media_attachments m ON m.id = j.media_id
            WHERE j.run_at <= now() AND NOT m.download_on_demand
            ORDER BY j.run_at
            LIMIT $1
            FOR UPDATE OF j SKIP LOCKED
        )
        RETURNING id, media_id, attempts
        "#,
        limit,
        PROCESSING_LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a finished (delivered, abandoned, or retry-exhausted) media job.
pub async fn complete_job(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM media_processing_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Removes the (unique) processing job for media that the synchronous proxy
/// cache-miss path finished first. Keeping the job until success makes a crash
/// or failed request retryable; deleting it afterwards prevents the background
/// worker from fetching bytes the proxy already stored.
pub async fn complete_job_for_media(pool: &PgPool, media_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM media_processing_jobs WHERE media_id = $1",
        media_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims and leases up to `limit` due on-demand A/V download jobs — the
/// complement of [`claim_due_processing`], run by the dedicated A/V lane.
pub async fn claim_due_on_demand(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedJob>, DbError> {
    // HLS-native playback is sparse and viewer-bound. Discard any legacy
    // whole-file job left queued across an upgrade before workers can claim
    // it, and restore the attachment's externally visible ready state.
    sqlx::query!(
        r#"
        WITH abandoned AS (
            DELETE FROM media_processing_jobs j
            USING media_attachments m
            WHERE m.id = j.media_id AND m.hls_master_url IS NOT NULL
            RETURNING j.media_id
        )
        UPDATE media_attachments SET processing = 'complete'
        WHERE id IN (SELECT media_id FROM abandoned) AND file_name IS NULL
        "#,
    )
    .execute(pool)
    .await?;
    let jobs = sqlx::query_as!(
        ClaimedJob,
        r#"
        UPDATE media_processing_jobs SET run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT j.id FROM media_processing_jobs j
            JOIN media_attachments m ON m.id = j.media_id
            WHERE j.run_at <= now() AND m.download_on_demand
            ORDER BY j.run_at
            LIMIT $1
            FOR UPDATE OF j SKIP LOCKED
        )
        RETURNING id, media_id, attempts
        "#,
        limit,
        ON_DEMAND_LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Re-queues a remote download that failed, after `delay`, recording the new
/// attempt count so the worker eventually gives up. Local transcodes never
/// reschedule. The job row is still present (the claim leased rather than
/// deleted it), so this updates it in place; a stale/missing row
/// (already completed) simply matches nothing.
pub async fn reschedule_job(
    pool: &PgPool,
    media_id: i64,
    attempts: i32,
    delay: std::time::Duration,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE media_processing_jobs
         SET run_at = now() + ($2 * interval '1 second'), attempts = $3
         WHERE media_id = $1",
        media_id,
        delay.as_secs_f64(),
        attempts,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The processed result the worker (or the synchronous v1 path) writes back.
#[derive(Debug)]
pub struct ProcessedMedia<'a> {
    pub media_id: i64,
    pub file_name: &'a str,
    pub content_type: &'a str,
    /// Re-detected Mastodon type: a soundless video becomes `gifv`, like
    /// Mastodon. Passive remote files use `None`; their API type is derived as
    /// `unknown` from the non-image/audio/video content type.
    pub kind: Option<&'a str>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub blurhash: Option<&'a str>,
    pub small: Option<SmallStyle<'a>>,
    pub duration: Option<f64>,
    pub frame_rate: Option<&'a str>,
    pub bitrate: Option<i64>,
}

/// Marks a queued attachment complete with its transcoded output.
pub async fn complete_processing(
    pool: &PgPool,
    done: ProcessedMedia<'_>,
) -> Result<Option<Media>, DbError> {
    let media = sqlx::query_as!(
        Media,
        r#"
        UPDATE media_attachments
        SET file_name = $2, content_type = $3, kind = $4, width = $5,
            height = $6, blurhash = $7, small_file_name = $8, small_width = $9,
            small_height = $10, duration = $11, frame_rate = $12, bitrate = $13,
            processing = 'complete',
            -- Mark when a remote file was cached so retention can evict it;
            -- local uploads keep cached_at NULL (never evicted).
            cached_at = CASE WHEN remote_url IS NOT NULL THEN now() ELSE cached_at END
        WHERE id = $1 AND processing = 'queued'
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        done.media_id,
        done.file_name,
        done.content_type,
        done.kind,
        done.width,
        done.height,
        done.blurhash,
        done.small.map(|s| s.file_name),
        done.small.map(|s| s.width),
        done.small.map(|s| s.height),
        done.duration,
        done.frame_rate,
        done.bitrate,
    )
    .fetch_optional(pool)
    .await?;
    Ok(media)
}

/// Records the stored byte sizes of an attachment's main file and thumbnail,
/// for the admin storage metrics (`instance_media_attachments` + `space_usage`).
/// Best-effort: callers invoke it after a successful store and ignore failures.
pub async fn set_file_sizes(
    pool: &PgPool,
    media_id: i64,
    file_size: i64,
    thumbnail_file_size: Option<i64>,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE media_attachments SET file_size = $2, thumbnail_file_size = $3 WHERE id = $1",
        media_id,
        file_size,
        thumbnail_file_size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`set_file_sizes`] over a whole backfill page in one statement.
pub async fn set_file_sizes_many(
    pool: &PgPool,
    media_ids: &[i64],
    file_sizes: &[i64],
    thumbnail_file_sizes: &[Option<i64>],
) -> Result<(), DbError> {
    if media_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE media_attachments m
         SET file_size = v.file_size, thumbnail_file_size = v.thumbnail_file_size
         FROM unnest($1::bigint[], $2::bigint[], $3::bigint[])
              AS v(id, file_size, thumbnail_file_size)
         WHERE m.id = v.id",
        media_ids,
        file_sizes,
        thumbnail_file_sizes as &[Option<i64>],
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks a queued attachment as permanently failed (clients get a 422 on
/// `GET /api/v1/media/{id}` and re-upload, like Mastodon).
pub async fn fail_processing(pool: &PgPool, media_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE media_attachments SET processing = 'failed' WHERE id = $1",
        media_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored file names a retention/orphan sweep freed, for the caller to
/// remove from the media store.
#[derive(Debug, Clone)]
pub struct ClearedFile {
    pub file_name: Option<String>,
    pub small_file_name: Option<String>,
}

/// Evicts cached remote media older than `retention`: removes the local-file
/// references (keeping the row, `remote_url` and metadata) and returns the file
/// names to delete from storage. Such rows redownload on demand the next time a
/// status using them is rendered (see [`enqueue_redownload`]). Up to `limit`.
pub async fn evict_cached_remote(
    pool: &PgPool,
    retention: std::time::Duration,
    limit: i64,
) -> Result<Vec<ClearedFile>, DbError> {
    let cleared = sqlx::query_as!(
        ClearedFile,
        r#"
        WITH due AS (
            SELECT id, file_name, small_file_name
            FROM media_attachments
            WHERE remote_url IS NOT NULL
              AND NOT download_on_demand
              AND cached_at IS NOT NULL
              AND cached_at < now() - ($1 * interval '1 second')
              -- A recently-served file is hot: eviction waits until viewers
              -- stop coming, however old the download is.
              AND (last_served_at IS NULL
                   OR last_served_at < now() - ($1 * interval '1 second'))
            ORDER BY cached_at
            LIMIT $2
        )
        UPDATE media_attachments m
        SET file_name = NULL, small_file_name = NULL, small_width = NULL,
            small_height = NULL, cached_at = NULL, last_served_at = NULL
        FROM due
        WHERE m.id = due.id
        RETURNING due.file_name AS "file_name?", due.small_file_name AS "small_file_name?"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(cleared)
}

/// Evicts every cached copy of remote media belonging to accounts on
/// `domain`, regardless of age — the domain-purge, matching Mastodon's
/// domain-media clearing service. Rows keep their
/// `remote_url`; with the block's `reject_media` in force (M31) they will
/// not be re-downloaded. Up to `limit`; call until empty.
/// The on-demand counterpart of [`evict_cached_remote`]: cached remote
/// *videos* (the big, cheap-to-refetch files) on their own retention clock.
pub async fn evict_cached_remote_video(
    pool: &PgPool,
    retention: std::time::Duration,
    limit: i64,
) -> Result<Vec<ClearedFile>, DbError> {
    let cleared = sqlx::query_as!(
        ClearedFile,
        r#"
        WITH due AS (
            SELECT id, file_name, small_file_name
            FROM media_attachments
            WHERE remote_url IS NOT NULL
              AND download_on_demand
              AND cached_at IS NOT NULL
              AND cached_at < now() - ($1 * interval '1 second')
              AND (last_served_at IS NULL
                   OR last_served_at < now() - ($1 * interval '1 second'))
            ORDER BY cached_at
            LIMIT $2
        )
        UPDATE media_attachments m
        SET file_name = NULL, small_file_name = NULL, small_width = NULL,
            small_height = NULL, cached_at = NULL, last_served_at = NULL
        FROM due
        WHERE m.id = due.id
        RETURNING due.file_name AS "file_name?", due.small_file_name AS "small_file_name?"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(cleared)
}

/// Total stored bytes of cached remote attachments in one retention class
/// (`on_demand` splits videos from the eager image cache) — the size-cap
/// sweep's measure.
pub async fn cached_remote_bytes(pool: &PgPool, on_demand: bool) -> Result<i64, DbError> {
    let total = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(SUM(COALESCE(file_size, 0) + COALESCE(thumbnail_file_size, 0)), 0)::bigint
            AS "total!"
        FROM media_attachments
        WHERE remote_url IS NOT NULL
          AND download_on_demand = $1
          AND cached_at IS NOT NULL
        "#,
        on_demand,
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Evicts the oldest cached remote attachments of one retention class
/// regardless of age — the size-cap sweep's knife. Recently-served files are
/// not exempt: when the cap is hit, something must go, and oldest-first is
/// the predictable choice.
pub async fn evict_cached_remote_oldest(
    pool: &PgPool,
    on_demand: bool,
    limit: i64,
) -> Result<Vec<ClearedFile>, DbError> {
    let cleared = sqlx::query_as!(
        ClearedFile,
        r#"
        WITH due AS (
            SELECT id, file_name, small_file_name
            FROM media_attachments
            WHERE remote_url IS NOT NULL
              AND download_on_demand = $1
              AND cached_at IS NOT NULL
            ORDER BY cached_at
            LIMIT $2
        )
        UPDATE media_attachments m
        SET file_name = NULL, small_file_name = NULL, small_width = NULL,
            small_height = NULL, cached_at = NULL, last_served_at = NULL
        FROM due
        WHERE m.id = due.id
        RETURNING due.file_name AS "file_name?", due.small_file_name AS "small_file_name?"
        "#,
        on_demand,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(cleared)
}

pub async fn evict_cached_for_domain(
    pool: &PgPool,
    domain: &str,
    limit: i64,
) -> Result<Vec<ClearedFile>, DbError> {
    let cleared = sqlx::query_as!(
        ClearedFile,
        r#"
        WITH due AS (
            SELECT m.id, m.file_name, m.small_file_name
            FROM media_attachments m
            JOIN accounts a ON a.id = m.account_id
            WHERE a.domain = $1
              AND m.remote_url IS NOT NULL
              AND (m.file_name IS NOT NULL OR m.small_file_name IS NOT NULL)
            LIMIT $2
        )
        UPDATE media_attachments m
        SET file_name = NULL, small_file_name = NULL, small_width = NULL,
            small_height = NULL, cached_at = NULL, last_served_at = NULL
        FROM due
        WHERE m.id = due.id
        RETURNING due.file_name AS "file_name?", due.small_file_name AS "small_file_name?"
        "#,
        domain,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(cleared)
}

/// Deletes unattached *local* uploads older than `ttl` (Mastodon's 1-day
/// orphan vacuum) and returns their files to remove. Remote rows and attached
/// uploads are never touched. Up to `limit`.
pub async fn vacuum_orphans(
    pool: &PgPool,
    ttl: std::time::Duration,
    limit: i64,
) -> Result<Vec<ClearedFile>, DbError> {
    let removed = sqlx::query_as!(
        ClearedFile,
        r#"
        WITH due AS (
            SELECT id, file_name, small_file_name
            FROM media_attachments
            WHERE status_id IS NULL AND scheduled_status_id IS NULL AND remote_url IS NULL
              AND created_at < now() - ($1 * interval '1 second')
            ORDER BY created_at
            LIMIT $2
        )
        DELETE FROM media_attachments m
        USING due
        WHERE m.id = due.id
        RETURNING due.file_name AS "file_name?", due.small_file_name AS "small_file_name?"
        "#,
        ttl.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(removed)
}

#[derive(Debug, Default)]
pub struct NewRemoteMedia<'a> {
    pub account_id: i64,
    pub status_id: i64,
    pub remote_url: &'a str,
    /// Preview card that discovered this media. Link-preview audio is exposed
    /// as a normal attachment, but remains distinguishable from media the
    /// author explicitly attached so a later text edit can recrawl it.
    pub preview_card_id: Option<i64>,
    pub content_type: &'a str,
    pub description: Option<&'a str>,
    pub blurhash: Option<&'a str>,
    pub focus: Option<(f64, f64)>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub thumbnail_remote_url: Option<&'a str>,
    /// `PeerTube` separated-audio companion stream (see [`Media::remote_audio_url`]).
    pub remote_audio_url: Option<&'a str>,
    /// Duration in seconds as declared by the AP object, shown until the
    /// cached copy is probed for the real value.
    pub duration: Option<f64>,
    /// Large A/V: skip the eager download; the media proxy enqueues it on
    /// the first play request instead (see [`enqueue_on_demand_download`]).
    pub download_on_demand: bool,
    /// The attachment skipped ingest-time processing because it arrived
    /// through remote history hydration. This is deliberately distinct from
    /// `download_on_demand`: cold images use the ordinary image cache lane on
    /// first view, while cold A/V remains play-triggered until promotion.
    pub history_deferred: bool,
    /// HLS-native remote video: the origin master playlist URL, kept so the
    /// caching HLS proxy can fetch + rewrite it on play. `None` for non-HLS.
    pub hls_master_url: Option<&'a str>,
    /// The full HLS rendition ladder (one per quality, plus the separated
    /// audio-only track as `height=0`/`is_audio`). Persisted only for HLS
    /// videos; empty otherwise. Authoritative for the quality selector.
    pub renditions: Vec<NewRendition<'a>>,
    /// `PeerTube` live broadcast state (`waiting`/`live`/`ended`); `None` for
    /// everything that is not a live.
    pub live_state: Option<&'a str>,
    /// Whether the live re-arms for a next broadcast instead of ending.
    pub live_permanent: bool,
}

/// One rendition of an HLS video's ladder, to persist alongside the media row.
#[derive(Debug, Clone)]
pub struct NewRendition<'a> {
    /// Display height in pixels; `0` marks the audio-only track.
    pub height: i32,
    pub width: Option<i32>,
    pub frame_rate: Option<i32>,
    pub size_bytes: Option<i64>,
    /// The rendition's fragmented mp4 on the origin.
    pub origin_url: &'a str,
    pub is_audio: bool,
}

/// A stored HLS rendition (read side): the ladder for a media attachment.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MediaRendition {
    pub media_id: i64,
    pub height: i32,
    pub width: Option<i32>,
    pub frame_rate: Option<i32>,
    pub size_bytes: Option<i64>,
    pub origin_url: String,
    pub is_audio: bool,
}

/// Stores a remote attachment reference and queues it for download +
/// re-processing (the worker caches it locally and re-detects its type, e.g.
/// gifv). Idempotent per (status, url): a duplicate inserts nothing and queues
/// no job. The metadata from the AP object (blurhash/dimensions/thumbnail) is
/// seeded as a placeholder shown until the download completes and overwrites it.
///
/// When the owner's domain has a `reject_media` block, the reference is still
/// stored (Mastodon keeps the record too) but never downloaded: the row is
/// born `complete` with no file, serving only its origin URL.
#[allow(
    clippy::too_many_lines,
    reason = "one transaction keeps the attachment, job, and rendition ladder atomic"
)]
pub async fn create_remote(pool: &PgPool, new: NewRemoteMedia<'_>) -> Result<(), DbError> {
    let (focus_x, focus_y) = match new.focus {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };
    let mut tx = pool.begin().await?;
    let rejects_media = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(instance_domain_rejects_media(domain), false) AS "rejects!"
        FROM accounts
        WHERE id = $1
        "#,
        new.account_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    let media_id = id::next();
    // A deferred row is born `complete` with no file. Intrinsic/on-history A/V
    // waits for the play-triggered lane; a history-deferred image waits for the
    // ordinary proxy cache-miss path. Neither may create ingest-time work.
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO media_attachments (id, account_id, status_id, remote_url,
                                       content_type, description, blurhash,
                                       focus_x, focus_y, width, height,
                                       thumbnail_remote_url, remote_audio_url,
                                       duration, download_on_demand,
                                       history_deferred,
                                       hls_master_url, live_state,
                                       live_permanent, preview_card_id,
                                       processing)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                $17, $18, $19, $20, $21,
                CASE WHEN $16 OR $15 OR $17 THEN 'complete' ELSE 'queued' END)
        ON CONFLICT (status_id, remote_url) WHERE remote_url IS NOT NULL DO NOTHING
        RETURNING id
        "#,
        media_id,
        new.account_id,
        new.status_id,
        new.remote_url,
        new.content_type,
        new.description,
        new.blurhash,
        focus_x,
        focus_y,
        new.width,
        new.height,
        new.thumbnail_remote_url,
        new.remote_audio_url,
        new.duration,
        new.download_on_demand,
        rejects_media,
        new.history_deferred,
        new.hls_master_url,
        new.live_state,
        new.live_permanent,
        new.preview_card_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_some() && !rejects_media && !new.download_on_demand && !new.history_deferred {
        sqlx::query!(
            "INSERT INTO media_processing_jobs (id, media_id) VALUES ($1, $2)",
            id::next(),
            media_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    // Persist the HLS rendition ladder (a new video row only; a conflict means
    // the attachment already existed with its ladder). The proxy range-caches
    // each rendition's fragmented mp4 independently and the built-in player
    // offers them as selectable qualities.
    if inserted.is_some() && !rejects_media && !new.renditions.is_empty() {
        let ids: Vec<i64> = new.renditions.iter().map(|_| id::next()).collect();
        let heights: Vec<i32> = new.renditions.iter().map(|r| r.height).collect();
        let widths: Vec<Option<i32>> = new.renditions.iter().map(|r| r.width).collect();
        let frame_rates: Vec<Option<i32>> = new.renditions.iter().map(|r| r.frame_rate).collect();
        let size_bytes: Vec<Option<i64>> = new.renditions.iter().map(|r| r.size_bytes).collect();
        let origin_urls: Vec<&str> = new.renditions.iter().map(|r| r.origin_url).collect();
        let is_audio: Vec<bool> = new.renditions.iter().map(|r| r.is_audio).collect();
        sqlx::query!(
            r#"
            INSERT INTO media_renditions
                (id, media_id, height, width, frame_rate, size_bytes,
                 origin_url, is_audio)
            SELECT v.id, $2, v.height, v.width, v.frame_rate, v.size_bytes,
                   v.origin_url, v.is_audio
            FROM unnest($1::bigint[], $3::int[], $4::int[], $5::int[],
                        $6::bigint[], $7::text[], $8::bool[])
                 AS v(id, height, width, frame_rate, size_bytes, origin_url, is_audio)
            ON CONFLICT (media_id, origin_url) DO NOTHING
            "#,
            &ids,
            media_id,
            &heights,
            &widths as &[Option<i32>],
            &frame_rates as &[Option<i32>],
            &size_bytes as &[Option<i64>],
            &origin_urls as &[&str],
            &is_audio,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Supplies inline HTML alt text when a structured attachment with the same
/// URL was inserted first but did not carry a description. Never overwrites a
/// description explicitly supplied by the attachment object.
pub async fn fill_remote_description(
    pool: &PgPool,
    status_id: i64,
    remote_url: &str,
    description: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE media_attachments
        SET description = $3
        WHERE status_id = $1 AND remote_url = $2 AND description IS NULL
        "#,
        status_id,
        remote_url,
        description,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A cold-history attachment starts as metadata-only. Once its status arrives
/// through the live delivery path, promote attachments deferred solely by
/// hydration and enqueue the normal processing job exactly once. Intrinsically
/// on-demand media (such as `PeerTube` HLS) stays on-demand. Instance media
/// rejection remains authoritative.
pub async fn promote_for_status(pool: &PgPool, status_id: i64) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let ids = sqlx::query_scalar!(
        r#"WITH promoted AS (
               UPDATE media_attachments m SET
                   download_on_demand = false,
                   history_deferred = false,
                   processing = CASE
                       WHEN m.file_name IS NULL
                        AND NOT COALESCE(instance_domain_rejects_media(a.domain), false)
                       THEN 'queued'
                       ELSE m.processing
                   END
               FROM accounts a
               WHERE m.status_id = $1
                 AND m.account_id = a.id
                 AND m.history_deferred
               RETURNING m.id, m.file_name,
                         COALESCE(instance_domain_rejects_media(a.domain), false) AS rejected
           )
           SELECT id FROM promoted WHERE file_name IS NULL AND NOT rejected"#,
        status_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    if !ids.is_empty() {
        let job_ids: Vec<i64> = ids.iter().map(|_| id::next()).collect();
        sqlx::query!(
            r#"INSERT INTO media_processing_jobs (id, media_id)
               SELECT input.job_id, input.media_id
               FROM unnest($1::bigint[], $2::bigint[]) input(job_id, media_id)
               ON CONFLICT (media_id) DO NOTHING"#,
            &job_ids,
            &ids,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(u64::try_from(ids.len()).unwrap_or(u64::MAX))
}

/// Applies a live broadcast's current state to its attachment, returning
/// whether anything actually changed.
///
/// The master playlist URL is only ever *learned*, never unlearned: a live is
/// announced before it has one, and a finished broadcast keeps advertising the
/// master it is about to delete. Passing `None` therefore leaves the stored
/// URL alone rather than clearing it, so a permanent live that is between
/// broadcasts keeps the address its next session will reuse.
pub async fn update_live(
    pool: &PgPool,
    media_id: i64,
    live_state: &str,
    hls_master_url: Option<&str>,
) -> Result<bool, DbError> {
    let changed = sqlx::query_scalar!(
        r#"
        UPDATE media_attachments
        SET live_state = $2,
            hls_master_url = COALESCE($3::text, hls_master_url)
        WHERE id = $1
          AND (live_state IS DISTINCT FROM $2
               OR ($3::text IS NOT NULL AND hls_master_url IS DISTINCT FROM $3::text))
        RETURNING id
        "#,
        media_id,
        live_state,
        hls_master_url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(changed.is_some())
}

/// Replaces a remote attachment's not-yet-cached poster URL.
///
/// Remote video producers can regenerate preview images while transcoding and
/// leave the URL advertised by their initial `Create` returning 404. Once a
/// preview (or the full file) is cached it is already usable, so this recovery
/// path deliberately changes only an attachment that still has neither.
pub async fn update_remote_thumbnail(
    pool: &PgPool,
    media_id: i64,
    thumbnail_remote_url: &str,
) -> Result<bool, DbError> {
    let changed = sqlx::query_scalar!(
        r#"
        UPDATE media_attachments
        SET thumbnail_remote_url = $2
        WHERE id = $1
          AND remote_url IS NOT NULL
          AND file_name IS NULL
          AND small_file_name IS NULL
          AND thumbnail_remote_url IS DISTINCT FROM $2
        RETURNING id
        "#,
        media_id,
        thumbnail_remote_url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(changed.is_some())
}

/// The HLS rendition ladders for a batch of media attachments, keyed by
/// `media_id` and ordered tallest-first (audio-only track last). Empty for
/// non-HLS media. Drives the quality selector + the HLS extension field.
pub async fn renditions_for(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<MediaRendition>>, DbError> {
    let rows = sqlx::query_as!(
        MediaRendition,
        r#"
        SELECT media_id, height, width, frame_rate, size_bytes, origin_url,
               is_audio
        FROM media_renditions
        WHERE media_id = ANY($1)
        ORDER BY is_audio ASC, height DESC, width DESC
        "#,
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    let mut by_media: HashMap<i64, Vec<MediaRendition>> = HashMap::new();
    for row in rows {
        by_media.entry(row.media_id).or_default().push(row);
    }
    Ok(by_media)
}

/// The origin HLS master playlist URL of one media attachment, if it is an
/// HLS-native video. Used by the caching HLS proxy to fetch + rewrite it.
pub async fn hls_master_url(pool: &PgPool, media_id: i64) -> Result<Option<String>, DbError> {
    let url = sqlx::query_scalar!(
        "SELECT hls_master_url FROM media_attachments WHERE id = $1",
        media_id,
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(url)
}

/// The stored HLS master URL for a status' video attachment, if any — the
/// edit-diffing signal: `PeerTube` regenerates the master (new UUID) whenever a
/// re-transcode changes the rendition ladder, so a differing master means the
/// video's HLS metadata must be re-ingested.
pub async fn hls_master_for_status(
    pool: &PgPool,
    status_id: i64,
) -> Result<Option<String>, DbError> {
    let url = sqlx::query_scalar!(
        r#"
        SELECT hls_master_url FROM media_attachments
        WHERE status_id = $1 AND hls_master_url IS NOT NULL
        LIMIT 1
        "#,
        status_id,
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(url)
}

/// A cached HLS segment: the stored file holding one byte range of a
/// rendition's fragmented mp4.
#[derive(Debug, Clone)]
pub struct HlsSegmentHit {
    pub cache_file: String,
    pub bytes: i64,
}

/// Looks up a cached HLS segment by its exact origin byte range. A hit means a
/// prior viewer already fetched it — this viewer (and any concurrent one) is
/// served from disk, so the origin sees a single fetch per segment.
pub async fn hls_segment_lookup(
    pool: &PgPool,
    origin_url: &str,
    range_start: i64,
    range_len: i64,
) -> Result<Option<HlsSegmentHit>, DbError> {
    let row = sqlx::query!(
        r#"
        SELECT cache_file, bytes FROM media_hls_segments
        WHERE origin_url = $1 AND range_start = $2 AND range_len = $3
        "#,
        origin_url,
        range_start,
        range_len,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| HlsSegmentHit {
        cache_file: r.cache_file,
        bytes: r.bytes,
    }))
}

/// Records a freshly cached segment. Returns `false` when a concurrent request
/// won the race (the caller should drop its now-duplicate cache file and use
/// the winner's).
#[allow(clippy::too_many_arguments)]
pub async fn hls_segment_record(
    pool: &PgPool,
    media_id: i64,
    origin_url: &str,
    range_start: i64,
    range_len: i64,
    cache_file: &str,
    bytes: i64,
) -> Result<bool, DbError> {
    let inserted = sqlx::query_scalar!(
        r#"
        INSERT INTO media_hls_segments
            (id, media_id, origin_url, range_start, range_len, cache_file, bytes)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (origin_url, range_start, range_len) DO NOTHING
        RETURNING id
        "#,
        id::next(),
        media_id,
        origin_url,
        range_start,
        range_len,
        cache_file,
        bytes,
    )
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// Drops one cached-segment row whose backing file turned out to be gone
/// (lost disk, out-of-band deletion): the serving path forgets it and
/// refetches instead of 404ing until the failure backoff runs out.
pub async fn hls_segment_forget(
    pool: &PgPool,
    origin_url: &str,
    range_start: i64,
    range_len: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        DELETE FROM media_hls_segments
        WHERE origin_url = $1 AND range_start = $2 AND range_len = $3
        "#,
        origin_url,
        range_start,
        range_len,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Touches a served segment's `last_served_at` (throttled to once an hour) so
/// retention keeps hot segments and evicts cold ones.
pub async fn hls_segment_touch(
    pool: &PgPool,
    origin_url: &str,
    range_start: i64,
    range_len: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE media_hls_segments SET last_served_at = now()
        WHERE origin_url = $1 AND range_start = $2 AND range_len = $3
          AND (last_served_at IS NULL OR last_served_at < now() - interval '1 hour')
        "#,
        origin_url,
        range_start,
        range_len,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Evicts cached HLS segments cold for at least `retention_secs` (not served,
/// or never served since caching, within the window), returning the cache
/// files to delete from the store. The cutoff is computed on the DB clock.
#[allow(clippy::cast_precision_loss)] // seconds fit f64 exactly at this scale
pub async fn hls_segments_evict(
    pool: &PgPool,
    retention_secs: i64,
) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        r#"
        DELETE FROM media_hls_segments
        WHERE cached_at < now() - ($1 * interval '1 second')
          AND (last_served_at IS NULL
               OR last_served_at < now() - ($1 * interval '1 second'))
        RETURNING cache_file
        "#,
        retention_secs as f64,
    )
    .fetch_all(pool)
    .await?;
    Ok(files)
}

/// Total stored bytes of cached HLS segments — counted into the video
/// class's size cap alongside the on-demand mp4s.
pub async fn hls_segments_bytes(pool: &PgPool) -> Result<i64, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(bytes), 0)::bigint AS "total!" FROM media_hls_segments"#,
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Evicts the least-recently-watched HLS segments regardless of age — the
/// video size-cap sweep's knife. Returns the freed cache file names.
pub async fn hls_segments_evict_lru(pool: &PgPool, limit: i64) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        r#"
        DELETE FROM media_hls_segments
        WHERE id IN (
            SELECT id FROM media_hls_segments
            ORDER BY COALESCE(last_served_at, cached_at)
            LIMIT $1
        )
        RETURNING cache_file
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(files)
}

/// Re-queues downloads for retention-evicted remote attachments (their files
/// are gone but the rows are still `complete`): flips them back to `queued`
/// and adds jobs, in one statement for the whole set — but only rows that
/// really are evicted remote rows with no job pending, so repeated serializer
/// hits never pile up jobs or disturb in-flight rows. A `reject_media` domain
/// block keeps a row serving its origin URL instead.
pub async fn enqueue_redownload<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    media_ids: &[i64],
) -> Result<(), DbError> {
    if media_ids.is_empty() {
        return Ok(());
    }
    let job_ids: Vec<i64> = media_ids.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        WITH flipped AS (
            UPDATE media_attachments m SET processing = 'queued'
            FROM accounts a
            WHERE m.id = ANY($2) AND a.id = m.account_id
              AND m.remote_url IS NOT NULL AND m.file_name IS NULL
              AND m.processing = 'complete'
              AND NOT m.download_on_demand
              AND NOT COALESCE(instance_domain_rejects_media(a.domain), false)
              AND NOT EXISTS (
                  SELECT 1 FROM media_fetch_failures f
                  WHERE f.kind = 'attachment' AND f.target_id = m.id
                    AND f.attempts >= $3
              )
            RETURNING m.id
        )
        INSERT INTO media_processing_jobs (id, media_id)
        SELECT j.job_id, f.id
        FROM flipped f
        JOIN unnest($1::bigint[], $2::bigint[]) AS j(job_id, media_id) ON j.media_id = f.id
        ON CONFLICT (media_id) DO NOTHING
        "#,
        &job_ids,
        media_ids,
        crate::media_fetch_failure::MAX_ATTEMPTS,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Queues the download of an on-demand A/V attachment — the play-triggered
/// counterpart of [`enqueue_redownload`], fired by the media proxy when a
/// viewer actually presses play (never at ingest, never on status render).
/// Idempotent under concurrent play requests: the row flip and the job-table
/// arbiter make the first request win and the rest no-ops.
pub async fn enqueue_on_demand_download(pool: &PgPool, media_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        WITH flipped AS (
            UPDATE media_attachments m SET processing = 'queued'
            FROM accounts a
            WHERE m.id = $2 AND a.id = m.account_id
              AND m.remote_url IS NOT NULL AND m.file_name IS NULL
              AND m.processing = 'complete'
              AND m.download_on_demand
              AND m.hls_master_url IS NULL
              AND NOT COALESCE(instance_domain_rejects_media(a.domain), false)
              AND NOT EXISTS (
                  SELECT 1 FROM media_fetch_failures f
                  WHERE f.kind = 'attachment' AND f.target_id = m.id
                    AND f.attempts >= $3
              )
            RETURNING m.id
        )
        INSERT INTO media_processing_jobs (id, media_id)
        SELECT $1, id FROM flipped
        ON CONFLICT (media_id) DO NOTHING
        "#,
        id::next(),
        media_id,
        crate::media_fetch_failure::MAX_ATTEMPTS,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records that the media proxy served (redirected a viewer to) the cached
/// copy, at most once an hour — retention uses it to spare hot files. Cheap
/// enough for the proxy's hot path: within the throttle window the UPDATE
/// matches no row.
pub async fn touch_served(pool: &PgPool, media_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE media_attachments SET last_served_at = now()
        WHERE id = $1 AND remote_url IS NOT NULL AND file_name IS NOT NULL
          AND (last_served_at IS NULL OR last_served_at < now() - interval '1 hour')
        "#,
        media_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Stores a poster/preview for a still-uncached remote attachment (the
/// thumbnail fetched from `thumbnail_remote_url` while the video itself waits
/// for its first play). Never touches a row that already has a full file —
/// completion writes its own, better poster extracted from the video.
pub async fn set_small_file(
    pool: &PgPool,
    media_id: i64,
    small: SmallStyle<'_>,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE media_attachments
        SET small_file_name = $2, small_width = $3, small_height = $4
        WHERE id = $1 AND file_name IS NULL AND remote_url IS NOT NULL
        "#,
        media_id,
        small.file_name,
        small.width,
        small.height,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Abandons a queued remote download without a file (a `reject_media` domain
/// block landed after the job was queued): the row completes serving only its
/// origin URL, exactly as if it had been created under the block.
pub async fn abandon_remote_download(pool: &PgPool, media_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE media_attachments SET processing = 'complete'
        WHERE id = $1 AND remote_url IS NOT NULL AND file_name IS NULL
          AND processing = 'queued'
        "#,
        media_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the remote attachment references of a status (edits rebuild
/// them; local uploads are never touched).
pub async fn delete_remote_for_status(pool: &PgPool, status_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM media_attachments
         WHERE status_id = $1 AND remote_url IS NOT NULL AND preview_card_id IS NULL",
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes media synthesized by link-preview discovery before an edited body
/// is crawled again. Explicitly attached media has no `preview_card_id` and is
/// never touched.
pub async fn delete_preview_media_for_status(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
) -> Result<(), DbError> {
    let removed = sqlx::query!(
        "DELETE FROM media_attachments
         WHERE status_id = $1 AND preview_card_id IS NOT NULL
         RETURNING file_name, small_file_name",
        status_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut stored_files = Vec::new();
    for row in removed {
        stored_files.extend(row.file_name);
        stored_files.extend(row.small_file_name);
    }
    crate::media_cleanup::enqueue_many(&mut *conn, &stored_files).await?;
    Ok(())
}

/// Attaches owned, still-unattached uploads to a status. Returns how many
/// rows matched — the caller fails the post when not all ids attached.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn attach<'e, E: PgExecutor<'e>>(
    executor: E,
    media_ids: &[i64],
    status_id: i64,
    account_id: i64,
) -> Result<usize, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE media_attachments SET status_id = $1
        WHERE id = ANY($2) AND account_id = $3 AND status_id IS NULL
        "#,
        status_id,
        media_ids,
        account_id,
    )
    .execute(executor)
    .await?;
    Ok(usize::try_from(result.rows_affected()).unwrap_or(0))
}

/// Reserves owned, unattached uploads for a scheduled status, so the
/// orphan-upload vacuum leaves them alone until the post is published (it then
/// attaches them normally and the scheduled row's deletion clears this link via
/// `ON DELETE SET NULL`). Mirrors Mastodon's `media_attachments.scheduled_status_id`.
pub async fn bind_to_scheduled(
    pool: &PgPool,
    media_ids: &[i64],
    scheduled_status_id: i64,
    account_id: i64,
) -> Result<usize, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE media_attachments SET scheduled_status_id = $1
        WHERE id = ANY($2) AND account_id = $3 AND status_id IS NULL
        "#,
        scheduled_status_id,
        media_ids,
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(usize::try_from(result.rows_affected()).unwrap_or(0))
}

/// Reconciles a local status' attachments to exactly `media_ids` (owned by
/// `account_id`): removed uploads are detached (back to unattached), new ones
/// attached. Returns false when an id was not attachable — the caller must
/// have validated, so that is a conflict to surface, not apply.
pub async fn set_attachments(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
    media_ids: &[i64],
) -> Result<bool, DbError> {
    let mut conn = pool.begin().await?;
    let result = set_attachments_conn(&mut conn, status_id, account_id, media_ids).await?;
    if result {
        conn.commit().await?;
    }
    Ok(result)
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn set_attachments_conn(
    conn: &mut sqlx::PgConnection,
    status_id: i64,
    account_id: i64,
    media_ids: &[i64],
) -> Result<bool, DbError> {
    // Link-preview media is derived, not an upload that can be reused on a
    // different post. Removing it deletes the row instead of turning it into
    // an immortal unattached remote-media record.
    let removed = sqlx::query!(
        "DELETE FROM media_attachments
         WHERE status_id = $1 AND account_id = $3
           AND preview_card_id IS NOT NULL AND NOT (id = ANY($2))
         RETURNING file_name, small_file_name",
        status_id,
        media_ids,
        account_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut stored_files = Vec::new();
    for row in removed {
        stored_files.extend(row.file_name);
        stored_files.extend(row.small_file_name);
    }
    crate::media_cleanup::enqueue_many(&mut *conn, &stored_files).await?;
    sqlx::query!(
        "UPDATE media_attachments SET status_id = NULL
         WHERE status_id = $1 AND account_id = $3
           AND preview_card_id IS NULL AND NOT (id = ANY($2))",
        status_id,
        media_ids,
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    let attached = sqlx::query!(
        r#"
        UPDATE media_attachments SET status_id = $1
        WHERE id = ANY($2) AND account_id = $3
          AND (status_id IS NULL OR status_id = $1)
        "#,
        status_id,
        media_ids,
        account_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(attached.rows_affected() == media_ids.len() as u64)
}

/// Attachments by id, in id order (edit-history rendering; rows deleted
/// since the snapshot are simply absent).
pub async fn find_by_ids(pool: &PgPool, media_ids: &[i64]) -> Result<Vec<Media>, DbError> {
    if media_ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query_as!(
        Media,
        r#"
        SELECT id, account_id, status_id, file_name, remote_url, content_type,
               description, width, height, created_at, kind, blurhash,
               focus_x, focus_y, small_file_name, small_width, small_height,
               thumbnail_remote_url, duration, frame_rate, bitrate, processing,
               remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        FROM media_attachments
        WHERE id = ANY($1)
        ORDER BY id
        "#,
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Updates the mutable attributes of an unattached upload (Mastodon's
/// `PUT /api/v1/media/{id}` only finds media without a status). `description`
/// distinguishes absent (keep) from present (replace, possibly with nothing);
/// a given `focus` always sets both axes.
pub async fn update_attributes(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
    description: Option<Option<&str>>,
    focus: Option<(f64, f64)>,
) -> Result<Option<Media>, DbError> {
    let (focus_x, focus_y) = match focus {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };
    let media = sqlx::query_as!(
        Media,
        r#"
        UPDATE media_attachments
        SET description = CASE WHEN $3 THEN $4 ELSE description END,
            focus_x = COALESCE($5, focus_x),
            focus_y = COALESCE($6, focus_y)
        WHERE id = $1 AND account_id = $2 AND status_id IS NULL
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        media_id,
        account_id,
        description.is_some(),
        description.flatten(),
        focus_x,
        focus_y,
    )
    .fetch_optional(pool)
    .await?;
    Ok(media)
}

/// [`update_attributes`] over the web composer's whole kept set in one
/// statement: parallel arrays of ids, whether to replace the description,
/// and the replacement (`NULL` clears). Only rows owned by `account_id` and
/// not yet attached to a status change, like the singular form; focus is
/// never touched (the composer has no focal-point control).
pub async fn update_attributes_many(
    pool: &PgPool,
    account_id: i64,
    media_ids: &[i64],
    set_description: &[bool],
    descriptions: &[Option<String>],
) -> Result<(), DbError> {
    if media_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"
        UPDATE media_attachments m
        SET description = CASE WHEN c.set_description THEN c.description
                               ELSE m.description END
        FROM unnest($2::bigint[], $3::boolean[], $4::text[])
             AS c(id, set_description, description)
        WHERE m.id = c.id AND m.account_id = $1 AND m.status_id IS NULL
        "#,
        account_id,
        media_ids,
        set_description as &[bool],
        descriptions as &[Option<String>],
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Stores the web composer's structured alternatives on an owned, unattached
/// upload. Transcript/audio-description state is replaced; an outer `None`
/// keeps existing captions while `Some(None)` clears them. Callers validate
/// `WebVTT` and length limits before this database boundary.
pub async fn update_accessibility(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
    update: AccessibilityUpdate<'_>,
) -> Result<bool, DbError> {
    let updated = sqlx::query_scalar!(
        r#"
        UPDATE media_attachments
        SET transcript = $3,
            caption_vtt = CASE WHEN $4 THEN $5 ELSE caption_vtt END,
            decorative = $6,
            audio_described = $7,
            visuals_conveyed_in_audio = $8
        WHERE id = $1 AND account_id = $2 AND status_id IS NULL
        RETURNING id
        "#,
        media_id,
        account_id,
        update.transcript,
        update.caption_vtt.is_some(),
        update.caption_vtt.flatten(),
        update.decorative,
        update.audio_described,
        update.visuals_conveyed_in_audio,
    )
    .fetch_optional(pool)
    .await?;
    Ok(updated.is_some())
}

/// Updates alternatives on an attachment after the status-edit path has
/// validated that it belongs to the edited local status. Captions are retained:
/// the urlencoded edit form cannot carry a replacement file.
pub async fn update_edit_accessibility(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
    update: AccessibilityUpdate<'_>,
) -> Result<bool, DbError> {
    let updated = sqlx::query_scalar!(
        r#"
        UPDATE media_attachments
        SET transcript = $3, decorative = $4, audio_described = $5,
            visuals_conveyed_in_audio = $6
        WHERE id = $1 AND account_id = $2
        RETURNING id
        "#,
        media_id,
        account_id,
        update.transcript,
        update.decorative,
        update.audio_described,
        update.visuals_conveyed_in_audio,
    )
    .fetch_optional(pool)
    .await?;
    Ok(updated.is_some())
}

/// The captions exposed by the public same-origin `WebVTT` route. Suspended
/// accounts fail closed exactly like their media bytes.
pub async fn public_caption_vtt(pool: &PgPool, media_id: i64) -> Result<Option<String>, DbError> {
    let captions = sqlx::query_scalar!(
        r#"
        SELECT m.caption_vtt
        FROM media_attachments m
        JOIN accounts a ON a.id = m.account_id
        WHERE m.id = $1 AND a.suspended_at IS NULL
        "#,
        media_id,
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(captions)
}

/// Updates the mutable attributes of an attachment during a status edit
/// (Mastodon's `PUT /api/v1/statuses/{id}` `media_attributes`). Unlike
/// [`update_attributes`] this reaches media already attached to a status —
/// the caller has validated that the attachment belongs to the edited status.
pub async fn update_edit_attributes(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
    description: Option<Option<&str>>,
    focus: Option<(f64, f64)>,
) -> Result<Option<Media>, DbError> {
    let (focus_x, focus_y) = match focus {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };
    let media = sqlx::query_as!(
        Media,
        r#"
        UPDATE media_attachments
        SET description = CASE WHEN $3 THEN $4 ELSE description END,
            focus_x = COALESCE($5, focus_x),
            focus_y = COALESCE($6, focus_y)
        WHERE id = $1 AND account_id = $2
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        media_id,
        account_id,
        description.is_some(),
        description.flatten(),
        focus_x,
        focus_y,
    )
    .fetch_optional(pool)
    .await?;
    Ok(media)
}

/// The set-based [`update_edit_attributes`]: applies one edit's whole
/// `media_attributes` list in a single statement. Parallel slices; a `true` in
/// `set_description` applies the paired description (`None` clears), `NULL`
/// focus components keep the stored value.
pub async fn update_edit_attributes_many<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    media_ids: &[i64],
    set_description: &[bool],
    descriptions: &[Option<String>],
    focus_x: &[Option<f64>],
    focus_y: &[Option<f64>],
) -> Result<(), DbError> {
    if media_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"
        UPDATE media_attachments m
        SET description = CASE WHEN c.set_description THEN c.description
                               ELSE m.description END,
            focus_x = COALESCE(c.focus_x, m.focus_x),
            focus_y = COALESCE(c.focus_y, m.focus_y)
        FROM unnest($2::bigint[], $3::boolean[], $4::text[], $5::float8[], $6::float8[])
             AS c(id, set_description, description, focus_x, focus_y)
        WHERE m.id = c.id AND m.account_id = $1
        "#,
        account_id,
        media_ids,
        set_description as &[bool],
        descriptions as &[Option<String>],
        focus_x as &[Option<f64>],
        focus_y as &[Option<f64>],
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_owned(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
) -> Result<Option<Media>, DbError> {
    let media = sqlx::query_as!(
        Media,
        r#"
        SELECT id, account_id, status_id, file_name, remote_url, content_type,
               description, width, height, created_at, kind, blurhash,
               focus_x, focus_y, small_file_name, small_width, small_height,
               thumbnail_remote_url, duration, frame_rate, bitrate, processing,
               remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        FROM media_attachments
        WHERE id = $1 AND account_id = $2
        "#,
        media_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(media)
}

/// [`find_owned`] over a whole id set in one statement — the post/edit paths
/// validate their requested attachment lists against it.
pub async fn find_owned_many(
    pool: &PgPool,
    media_ids: &[i64],
    account_id: i64,
) -> Result<Vec<Media>, DbError> {
    if media_ids.is_empty() {
        return Ok(Vec::new());
    }
    let media = sqlx::query_as!(
        Media,
        r#"
        SELECT id, account_id, status_id, file_name, remote_url, content_type,
               description, width, height, created_at, kind, blurhash,
               focus_x, focus_y, small_file_name, small_width, small_height,
               thumbnail_remote_url, duration, frame_rate, bitrate, processing,
               remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        FROM media_attachments
        WHERE id = ANY($1) AND account_id = $2
        "#,
        media_ids,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(media)
}

/// Deletes an owned, unattached upload, returning the row (the caller
/// removes the stored files). Attached media never match — Mastodon answers
/// those with a 422 instead.
pub async fn delete_unattached(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
) -> Result<Option<Media>, DbError> {
    let media = sqlx::query_as!(
        Media,
        r#"
        DELETE FROM media_attachments
        WHERE id = $1 AND account_id = $2 AND status_id IS NULL
        RETURNING id, account_id, status_id, file_name, remote_url, content_type,
                  description, width, height, created_at, kind, blurhash,
                  focus_x, focus_y, small_file_name, small_width, small_height,
                  thumbnail_remote_url, duration, frame_rate, bitrate, processing,
                  remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        "#,
        media_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(media)
}

/// All attachments for a batch of statuses, grouped by status id.
pub async fn for_statuses(
    pool: &PgPool,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<Media>>, DbError> {
    let mut conn = pool.acquire().await?;
    for_statuses_conn(&mut conn, status_ids).await
}

/// Connection-scoped variant for callers assembling a transactional mutation.
pub async fn for_statuses_conn(
    conn: &mut sqlx::PgConnection,
    status_ids: &[i64],
) -> Result<HashMap<i64, Vec<Media>>, DbError> {
    let rows = sqlx::query_as!(
        Media,
        r#"
        SELECT id, account_id, status_id, file_name, remote_url, content_type,
               description, width, height, created_at, kind, blurhash,
               focus_x, focus_y, small_file_name, small_width, small_height,
               thumbnail_remote_url, duration, frame_rate, bitrate, processing,
               remote_audio_url, download_on_demand, live_state, live_permanent,
                  hls_master_url, transcript, caption_vtt, decorative,
                  audio_described, visuals_conveyed_in_audio
        FROM media_attachments
        WHERE status_id = ANY($1)
        ORDER BY id
        "#,
        status_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    // Retention-evicted remote attachments (file gone, row still complete) are
    // re-queued for download on demand — here, as a status that uses them is
    // rendered, one statement for the whole page. Rare, and self-limiting: the
    // flip to `queued` stops re-firing.
    let evicted: Vec<i64> = rows
        .iter()
        // On-demand A/V never re-downloads from a render — a timeline scroll
        // must not re-pull a gigabyte video; its first play does that.
        .filter(|r| {
            r.remote_url.is_some()
                && r.file_name.is_none()
                && r.processing == "complete"
                && !r.download_on_demand
        })
        .map(|r| r.id)
        .collect();
    enqueue_redownload(&mut *conn, &evicted).await?;
    let mut map: HashMap<i64, Vec<Media>> = HashMap::new();
    for row in rows {
        if let Some(status_id) = row.status_id {
            map.entry(status_id).or_default().push(row);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    async fn fixture(pool: &PgPool) -> (i64, i64) {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let post = status::create_local(
            pool,
            status::NewLocalStatus::new(account.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();
        (account.id, post.id)
    }

    fn upload(account_id: i64, media_id: i64) -> NewLocalMedia<'static> {
        NewLocalMedia {
            description: None,
            width: Some(10),
            height: Some(10),
            ..NewLocalMedia::new(account_id, media_id, "x.jpg", "image/jpeg")
        }
    }

    async fn pending_jobs(pool: &PgPool) -> i64 {
        sqlx::query_scalar!(r#"SELECT count(*) AS "c!" FROM media_processing_jobs"#)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A media processing job survives a crash: the claim leases the row rather
    /// than deleting it, so a process that dies before completing the work has
    /// the job resurface once the lease expires, instead of leaving the
    /// attachment stuck `queued` with no job.
    #[sqlx::test]
    async fn processing_job_is_leased_and_survives_a_crash(pool: PgPool) {
        let (account_id, _status_id) = fixture(&pool).await;
        let media_id = id::next();
        create_queued(
            &pool,
            NewQueuedMedia {
                account_id,
                media_id,
                file_name: "x.orig",
                content_type: "image/jpeg",
                kind: "image",
                description: None,
                focus: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(pending_jobs(&pool).await, 1);

        // Claiming leases (does not delete) the job.
        let jobs = claim_due_processing(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].media_id, media_id);
        assert_eq!(jobs[0].attempts, 0);
        // The row survives a crash before completion, and is not re-claimable
        // while the lease holds.
        assert_eq!(pending_jobs(&pool).await, 1);
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());

        // Simulate a crash (never completed) plus lease expiry: the same job
        // resurfaces with its retry budget intact — the claim does not spend
        // `attempts`, only an explicit reschedule does.
        sqlx::query!("UPDATE media_processing_jobs SET run_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let again = claim_due_processing(&pool, 10).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, jobs[0].id);
        assert_eq!(again[0].attempts, 0, "a crash reclaim keeps the budget");

        // Completing removes it for good.
        complete_job(&pool, again[0].id).await.unwrap();
        assert_eq!(pending_jobs(&pool).await, 0);
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn upload_attach_and_render_lifecycle(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        let media_id = id::next();
        let media = create_local(&pool, upload(account_id, media_id))
            .await
            .unwrap();
        assert_eq!(media.id, media_id);
        assert!(media.status_id.is_none());
        assert_eq!(media.processing, "complete");
        assert!(!media.not_processed());

        // Attach validates ownership and one-shot use.
        assert_eq!(
            attach(&pool, &[media_id], status_id, account_id + 1)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            attach(&pool, &[media_id], status_id, account_id)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            attach(&pool, &[media_id], status_id, account_id)
                .await
                .unwrap(),
            0
        );

        let grouped = for_statuses(&pool, &[status_id]).await.unwrap();
        assert_eq!(grouped[&status_id].len(), 1);
        assert_eq!(grouped[&status_id][0].file_name.as_deref(), Some("x.jpg"));
    }

    #[sqlx::test]
    async fn update_attributes_merges_and_scopes_to_unattached(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        let media_id = id::next();
        create_local(&pool, upload(account_id, media_id))
            .await
            .unwrap();

        // Setting only the focus keeps the description (and vice versa).
        let updated = update_attributes(
            &pool,
            media_id,
            account_id,
            Some(Some("alt text")),
            Some((-0.5, 0.25)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.description.as_deref(), Some("alt text"));
        let updated = update_attributes(&pool, media_id, account_id, None, Some((0.1, 0.2)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.description.as_deref(), Some("alt text"));
        assert_eq!((updated.focus_x, updated.focus_y), (Some(0.1), Some(0.2)));
        let updated = update_attributes(&pool, media_id, account_id, Some(None), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.description, None);
        assert_eq!(updated.focus_x, Some(0.1));

        // Attached media are out of scope, like Mastodon's controller.
        attach(&pool, &[media_id], status_id, account_id)
            .await
            .unwrap();
        assert!(
            update_attributes(&pool, media_id, account_id, Some(Some("x")), None)
                .await
                .unwrap()
                .is_none()
        );
        // ... and cannot be deleted either.
        assert!(
            delete_unattached(&pool, media_id, account_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn accessibility_alternatives_are_owned_and_public_until_suspension(pool: PgPool) {
        let (account_id, _status_id) = fixture(&pool).await;
        let media_id = id::next();
        create_local(&pool, upload(account_id, media_id))
            .await
            .unwrap();
        let vtt = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHello\n";

        assert!(
            update_accessibility(
                &pool,
                media_id,
                account_id,
                AccessibilityUpdate {
                    transcript: Some("A transcript"),
                    caption_vtt: Some(Some(vtt)),
                    decorative: true,
                    audio_described: true,
                    visuals_conveyed_in_audio: false,
                },
            )
            .await
            .unwrap()
        );
        assert!(
            !update_accessibility(
                &pool,
                media_id,
                account_id + 1,
                AccessibilityUpdate::default(),
            )
            .await
            .unwrap()
        );

        let stored = find_by_ids(&pool, &[media_id]).await.unwrap().remove(0);
        assert_eq!(stored.transcript.as_deref(), Some("A transcript"));
        assert!(stored.decorative);
        assert!(stored.audio_described);
        assert!(!stored.visuals_conveyed_in_audio);
        assert_eq!(
            public_caption_vtt(&pool, media_id)
                .await
                .unwrap()
                .as_deref(),
            Some(vtt)
        );

        sqlx::query!(
            "UPDATE accounts SET suspended_at = now() WHERE id = $1",
            account_id
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(public_caption_vtt(&pool, media_id).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn set_attachments_reconciles_additions_and_removals(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        let kept = id::next();
        let removed = id::next();
        let added = id::next();
        for media_id in [kept, removed, added] {
            create_local(&pool, upload(account_id, media_id))
                .await
                .unwrap();
        }
        attach(&pool, &[kept, removed], status_id, account_id)
            .await
            .unwrap();
        let card_id = id::next();
        sqlx::query!(
            "INSERT INTO preview_cards (id, url) VALUES ($1, 'https://pod.example/episode')",
            card_id,
        )
        .execute(&pool)
        .await
        .unwrap();
        create_remote(
            &pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: "https://pod.example/episode.mp3",
                preview_card_id: Some(card_id),
                content_type: "audio/mpeg",
                download_on_demand: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE media_attachments
             SET file_name = 'cached-audio.mp3', small_file_name = 'cached-cover.avif'
             WHERE status_id = $1 AND preview_card_id = $2",
            status_id,
            card_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            set_attachments(&pool, status_id, account_id, &[kept, added])
                .await
                .unwrap()
        );
        let grouped = for_statuses(&pool, &[status_id]).await.unwrap();
        let ids: Vec<i64> = grouped[&status_id].iter().map(|m| m.id).collect();
        assert_eq!(ids, [kept, added]);
        // The removed upload is unattached again, not deleted.
        let detached = find_owned(&pool, removed, account_id).await.unwrap();
        assert_eq!(detached.unwrap().status_id, None);
        let cleanup_files =
            sqlx::query_scalar!("SELECT file_name FROM media_cleanup_jobs ORDER BY file_name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(cleanup_files, ["cached-audio.mp3", "cached-cover.avif"]);

        // Someone else's upload does not attach.
        assert!(
            !set_attachments(&pool, status_id, account_id + 1, &[kept])
                .await
                .unwrap()
        );

        assert_eq!(
            find_by_ids(&pool, &[kept, added, 12345])
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[sqlx::test]
    async fn remote_media_dedups_per_status(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        for _ in 0..2 {
            create_remote(
                &pool,
                NewRemoteMedia {
                    account_id,
                    status_id,
                    remote_url: "https://remote.example/media/1.png",
                    content_type: "image/png",
                    description: Some("pic"),
                    blurhash: Some("UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of"),
                    focus: Some((0.0, -0.5)),
                    width: Some(640),
                    height: Some(480),
                    thumbnail_remote_url: Some("https://remote.example/media/1_thumb.png"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let grouped = for_statuses(&pool, &[status_id]).await.unwrap();
        assert_eq!(grouped[&status_id].len(), 1);
        let stored = &grouped[&status_id][0];
        assert_eq!(
            stored.blurhash.as_deref(),
            Some("UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of")
        );
        assert_eq!((stored.focus_x, stored.focus_y), (Some(0.0), Some(-0.5)));
        assert_eq!((stored.width, stored.height), (Some(640), Some(480)));
        assert_eq!(
            stored.thumbnail_remote_url.as_deref(),
            Some("https://remote.example/media/1_thumb.png")
        );
        // Remote rows are queued for download + re-processing, and exactly one
        // download job is enqueued (the duplicate insert queues nothing).
        assert!(stored.not_processed());
        assert_eq!(stored.processing, "queued");
        assert_eq!(claim_due_processing(&pool, 10).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn remote_thumbnail_can_rotate_until_a_preview_is_cached(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        create_remote(
            &pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: "https://remote.example/video.mp4",
                content_type: "video/mp4",
                thumbnail_remote_url: Some("https://remote.example/stale.jpg"),
                download_on_demand: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let media_id = for_statuses(&pool, &[status_id]).await.unwrap()[&status_id][0].id;

        assert!(
            update_remote_thumbnail(&pool, media_id, "https://remote.example/current.jpg")
                .await
                .unwrap()
        );
        let refreshed = &find_by_ids(&pool, &[media_id]).await.unwrap()[0];
        assert_eq!(
            refreshed.thumbnail_remote_url.as_deref(),
            Some("https://remote.example/current.jpg")
        );

        set_small_file(
            &pool,
            media_id,
            SmallStyle {
                file_name: "cached-small.avif",
                width: 320,
                height: 180,
            },
        )
        .await
        .unwrap();
        assert!(
            !update_remote_thumbnail(&pool, media_id, "https://remote.example/later.jpg")
                .await
                .unwrap(),
            "a healthy cached preview is immutable"
        );
    }

    #[sqlx::test]
    async fn queued_media_lifecycle(pool: PgPool) {
        let (account_id, _) = fixture(&pool).await;
        let media_id = id::next();
        let queued = create_queued(
            &pool,
            NewQueuedMedia {
                account_id,
                media_id,
                file_name: &format!("{media_id}.orig"),
                content_type: "video/webm",
                kind: "video",
                description: Some("clip"),
                focus: None,
            },
        )
        .await
        .unwrap();
        assert!(queued.not_processed());

        // The job is claimable exactly once.
        let claimed = claim_due_processing(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].media_id, media_id);
        assert_eq!(claimed[0].attempts, 0);
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());

        let done = complete_processing(
            &pool,
            ProcessedMedia {
                media_id,
                file_name: &format!("{media_id}.mp4"),
                content_type: "video/mp4",
                kind: Some("gifv"),
                width: Some(320),
                height: Some(240),
                blurhash: Some("UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of"),
                small: Some(SmallStyle {
                    file_name: &format!("{media_id}.small.png"),
                    width: 320,
                    height: 240,
                }),
                duration: Some(1.5),
                frame_rate: Some("30/1"),
                bitrate: Some(128_000),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(done.processing, "complete");
        assert_eq!(done.kind.as_deref(), Some("gifv"));
        assert_eq!(done.content_type, "video/mp4");
        assert_eq!(done.description.as_deref(), Some("clip"));
        assert_eq!(done.duration, Some(1.5));
        assert_eq!(done.frame_rate.as_deref(), Some("30/1"));

        // Completing twice is a no-op (the row is no longer queued).
        assert!(
            complete_processing(
                &pool,
                ProcessedMedia {
                    media_id,
                    file_name: "x",
                    content_type: "x",
                    kind: Some("video"),
                    width: None,
                    height: None,
                    blurhash: None,
                    small: None,
                    duration: None,
                    frame_rate: None,
                    bitrate: None,
                },
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    /// Drives a remote attachment through download + caching for the
    /// retention tests: queued → claimed → completed with files + `cached_at`.
    async fn cached_remote(pool: &PgPool, account_id: i64, status_id: i64, url: &str) -> i64 {
        create_remote(
            pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: url,
                content_type: "image/png",
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Mimic the worker: claim (leases) the job, write the result, then
        // complete (delete) the job — leaving the queue drained, as a real
        // successful download does.
        let job = claim_due_processing(pool, 1).await.unwrap()[0];
        let media_id = job.media_id;
        complete_processing(
            pool,
            ProcessedMedia {
                media_id,
                file_name: &format!("{media_id}.jpg"),
                content_type: "image/jpeg",
                kind: Some("image"),
                width: Some(10),
                height: Some(10),
                blurhash: Some("UBL_:rOpGG-;~qRjWBay0fI]%2s:S$M{R*of"),
                small: Some(SmallStyle {
                    file_name: &format!("{media_id}.small.jpg"),
                    width: 5,
                    height: 5,
                }),
                duration: None,
                frame_rate: None,
                bitrate: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        complete_job(pool, job.id).await.unwrap();
        media_id
    }

    #[sqlx::test]
    async fn retention_evicts_then_redownloads_remote(pool: PgPool) {
        use std::time::Duration;
        let (account_id, status_id) = fixture(&pool).await;
        let media_id = cached_remote(&pool, account_id, status_id, "https://r.example/1.png").await;
        let second_id =
            cached_remote(&pool, account_id, status_id, "https://r.example/2.png").await;

        // A wide retention window evicts nothing.
        assert!(
            evict_cached_remote(&pool, Duration::from_hours(1), 10)
                .await
                .unwrap()
                .is_empty()
        );
        // Backdate the caches so they are past a one-day retention.
        sqlx::query!(
            "UPDATE media_attachments SET cached_at = now() - interval '10 days' WHERE id = ANY($1)",
            &[media_id, second_id][..],
        )
        .execute(&pool)
        .await
        .unwrap();

        let cleared = evict_cached_remote(&pool, Duration::from_hours(24), 10)
            .await
            .unwrap();
        assert_eq!(cleared.len(), 2);
        assert!(cleared.iter().any(|c| c.file_name.as_deref()
            == Some(format!("{media_id}.jpg").as_str())
            && c.small_file_name.as_deref() == Some(format!("{media_id}.small.jpg").as_str())));
        assert!(
            cleared
                .iter()
                .any(|c| c.file_name.as_deref() == Some(format!("{second_id}.jpg").as_str()))
        );

        // The row keeps remote_url + metadata, loses its file and cache mark.
        let row = find_by_ids(&pool, &[media_id]).await.unwrap();
        let row = &row[0];
        assert!(row.file_name.is_none());
        assert!(row.small_file_name.is_none());
        assert!(row.remote_url.is_some());
        assert!(row.blurhash.is_some());
        assert_eq!(row.processing, "complete");

        // Rendering the status re-queues both downloads, exactly once each —
        // the whole evicted page goes through one batched enqueue.
        for_statuses(&pool, &[status_id]).await.unwrap();
        for_statuses(&pool, &[status_id]).await.unwrap();
        let claimed = claim_due_processing(&pool, 10).await.unwrap();
        let mut claimed_ids: Vec<i64> = claimed.iter().map(|c| c.media_id).collect();
        claimed_ids.sort_unstable();
        let mut expected = vec![media_id, second_id];
        expected.sort_unstable();
        assert_eq!(claimed_ids, expected);
        let row = find_by_ids(&pool, &[media_id]).await.unwrap();
        assert_eq!(row[0].processing, "queued");
    }

    #[sqlx::test]
    async fn retention_spares_recently_served_files(pool: PgPool) {
        use std::time::Duration;
        let (account_id, status_id) = fixture(&pool).await;
        let media_id = cached_remote(&pool, account_id, status_id, "https://r.example/2.png").await;
        sqlx::query!(
            "UPDATE media_attachments SET cached_at = now() - interval '10 days' WHERE id = $1",
            media_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // A viewer just reached the file: eviction must wait, however old the
        // download is.
        touch_served(&pool, media_id).await.unwrap();
        assert!(
            evict_cached_remote(&pool, Duration::from_hours(24), 10)
                .await
                .unwrap()
                .is_empty(),
            "a hot file is not evicted"
        );
        // Repeated serves within the throttle window don't rewrite the row.
        let before = sqlx::query_scalar!(
            "SELECT last_served_at FROM media_attachments WHERE id = $1",
            media_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        touch_served(&pool, media_id).await.unwrap();
        let after = sqlx::query_scalar!(
            "SELECT last_served_at FROM media_attachments WHERE id = $1",
            media_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(before, after, "touch throttles to once an hour");

        // Once viewers stop coming, the old download evicts normally.
        sqlx::query!(
            "UPDATE media_attachments SET last_served_at = now() - interval '10 days' WHERE id = $1",
            media_id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            evict_cached_remote(&pool, Duration::from_hours(24), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test]
    async fn on_demand_rows_queue_only_from_play(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        create_remote(
            &pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: "https://r.example/video-720.mp4",
                content_type: "video/mp4",
                remote_audio_url: Some("https://r.example/video-audio.mp4"),
                duration: Some(10_185.0),
                download_on_demand: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let grouped = for_statuses(&pool, &[status_id]).await.unwrap();
        let row = &grouped[&status_id][0];
        assert_eq!(row.processing, "complete", "born serving origin");
        assert_eq!(
            row.remote_audio_url.as_deref(),
            Some("https://r.example/video-audio.mp4")
        );
        assert_eq!(row.duration, Some(10_185.0));
        // Neither ingest nor rendering queued anything.
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());
        assert!(claim_due_on_demand(&pool, 10).await.unwrap().is_empty());

        // Concurrent play requests queue exactly one job, for the A/V lane.
        enqueue_on_demand_download(&pool, row.id).await.unwrap();
        enqueue_on_demand_download(&pool, row.id).await.unwrap();
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());
        let claimed = claim_due_on_demand(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].media_id, row.id);
        let row = &find_by_ids(&pool, &[row.id]).await.unwrap()[0];
        assert_eq!(row.processing, "queued");

        // The eager redownload path (status render) never touches it.
        abandon_remote_download(&pool, row.id).await.unwrap();
        enqueue_redownload(&pool, &[row.id]).await.unwrap();
        assert!(claim_due_on_demand(&pool, 10).await.unwrap().is_empty());
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn hls_rows_never_enter_the_whole_file_lane(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        create_remote(
            &pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: "https://r.example/video-fragmented.mp4",
                content_type: "video/mp4",
                hls_master_url: Some("https://r.example/master.m3u8"),
                download_on_demand: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let row = &for_statuses(&pool, &[status_id]).await.unwrap()[&status_id][0];
        enqueue_on_demand_download(&pool, row.id).await.unwrap();
        assert!(claim_due_on_demand(&pool, 10).await.unwrap().is_empty());
        assert_eq!(
            find_by_ids(&pool, &[row.id]).await.unwrap()[0].processing,
            "complete"
        );

        // Simulate a job left behind by a server running the old code during
        // a rolling upgrade: the claimant discards it instead of hammering the
        // origin after deployment.
        sqlx::query!(
            "UPDATE media_attachments SET processing = 'queued' WHERE id = $1",
            row.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO media_processing_jobs (id, media_id) VALUES ($1, $2)",
            id::next(),
            row.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(claim_due_on_demand(&pool, 10).await.unwrap().is_empty());
        assert_eq!(
            find_by_ids(&pool, &[row.id]).await.unwrap()[0].processing,
            "complete"
        );
    }

    #[sqlx::test]
    async fn orphan_vacuum_removes_aged_unattached_uploads(pool: PgPool) {
        use std::time::Duration;
        let (account_id, status_id) = fixture(&pool).await;
        let orphan = id::next();
        let attached = id::next();
        create_local(&pool, upload(account_id, orphan))
            .await
            .unwrap();
        create_local(&pool, upload(account_id, attached))
            .await
            .unwrap();
        attach(&pool, &[attached], status_id, account_id)
            .await
            .unwrap();

        // Fresh uploads are not orphaned.
        assert!(
            vacuum_orphans(&pool, Duration::from_hours(24), 10)
                .await
                .unwrap()
                .is_empty()
        );
        // Age both; only the unattached one is swept.
        sqlx::query!("UPDATE media_attachments SET created_at = now() - interval '2 days'")
            .execute(&pool)
            .await
            .unwrap();
        let removed = vacuum_orphans(&pool, Duration::from_hours(24), 10)
            .await
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].file_name.as_deref(), Some("x.jpg"));
        assert!(find_by_ids(&pool, &[orphan]).await.unwrap().is_empty());
        assert_eq!(find_by_ids(&pool, &[attached]).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn reject_media_domain_never_downloads(pool: PgPool) {
        use crate::account::RemoteAccountData;
        use crate::instance_policy::{self, NewDomainBlock};

        instance_policy::create_domain_block(
            &pool,
            NewDomainBlock {
                domain: "blocked.example",
                severity: "noop",
                reject_media: true,
                reject_reports: false,
                private_comment: None,
                public_comment: None,
                obfuscate: false,
            },
        )
        .await
        .unwrap();

        let remote = |username: &'static str, domain: &'static str| {
            let pool = pool.clone();
            async move {
                account::upsert_remote(
                    &pool,
                    RemoteAccountData {
                        username,
                        domain,
                        uri: &format!("https://{domain}/users/{username}"),
                        display_name: "",
                        note: "",
                        inbox_url: &format!("https://{domain}/users/{username}/inbox"),
                        shared_inbox_url: &format!("https://{domain}/inbox"),
                        public_key_pem: "pub",
                        public_key_id: &format!("https://{domain}/users/{username}#main-key"),
                        avatar_remote_url: None,
                        header_remote_url: None,
                        avatar_description: "",
                        header_description: "",
                        created_at: None,
                        fields: Vec::new(),
                        featured_collection_url: None,
                        locked: false,
                        also_known_as: &[],
                        moved_to_uri: None,
                        url: None,
                        discoverable: false,
                        feature_approval_policy: 0,
                        is_bot: false,
                        indexable: false,
                        show_media: None,
                        show_media_replies: None,
                        show_featured: None,
                        memorial: false,
                        actor_type: None,
                    },
                )
                .await
                .unwrap()
                .id
            }
        };
        let post = |account_id, uri: &'static str| {
            let pool = pool.clone();
            async move {
                status::upsert_remote(
                    &pool,
                    status::NewRemoteStatus {
                        title: None,
                        object_type: None,
                        external_url: None,
                        account_id,
                        uri,
                        content: "<p>post</p>",
                        created_at: time::OffsetDateTime::now_utc(),
                        visibility: "public",
                        in_reply_to_id: None,
                        in_reply_to_uri: None,
                        spoiler_text: "",
                        sensitive: false,
                        language: None,
                        url: None,
                        quote_approval_policy: 0,
                    },
                )
                .await
                .unwrap()
                .id
            }
        };
        let attachment = |account_id, status_id, url: &'static str| {
            let pool = pool.clone();
            async move {
                create_remote(
                    &pool,
                    NewRemoteMedia {
                        account_id,
                        status_id,
                        remote_url: url,
                        content_type: "image/png",
                        description: None,
                        blurhash: None,
                        focus: None,
                        thumbnail_remote_url: None,
                        width: None,
                        height: None,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
        };

        let blocked = remote("bad", "blocked.example").await;
        let blocked_status = post(blocked, "https://blocked.example/users/bad/statuses/1").await;
        attachment(
            blocked,
            blocked_status,
            "https://blocked.example/media/1.png",
        )
        .await;
        // The reference is stored (serving its origin URL) but never queued.
        let grouped = for_statuses(&pool, &[blocked_status]).await.unwrap();
        assert_eq!(grouped[&blocked_status].len(), 1);
        let stored = &grouped[&blocked_status][0];
        assert_eq!(stored.processing, "complete");
        assert!(stored.file_name.is_none());
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());
        // Neither the serializer path (`for_statuses` above) nor an explicit
        // redownload may re-queue it.
        enqueue_redownload(&pool, &[stored.id]).await.unwrap();
        assert!(claim_due_processing(&pool, 10).await.unwrap().is_empty());

        // A clean domain still queues its download.
        let clean = remote("ok", "clean.example").await;
        let clean_status = post(clean, "https://clean.example/users/ok/statuses/1").await;
        attachment(clean, clean_status, "https://clean.example/media/1.png").await;
        let claimed = claim_due_processing(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[sqlx::test]
    async fn abandon_remote_download_completes_without_a_file(pool: PgPool) {
        let (account_id, status_id) = fixture(&pool).await;
        create_remote(
            &pool,
            NewRemoteMedia {
                account_id,
                status_id,
                remote_url: "https://remote.example/media/9.png",
                content_type: "image/png",
                description: None,
                blurhash: None,
                focus: None,
                thumbnail_remote_url: None,
                width: None,
                height: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let media_id = claim_due_processing(&pool, 10).await.unwrap()[0].media_id;
        abandon_remote_download(&pool, media_id).await.unwrap();
        let row = &find_by_ids(&pool, &[media_id]).await.unwrap()[0];
        assert_eq!(row.processing, "complete");
        assert!(row.file_name.is_none());
    }

    #[sqlx::test]
    async fn failed_processing_is_recorded(pool: PgPool) {
        let (account_id, _) = fixture(&pool).await;
        let media_id = id::next();
        create_queued(
            &pool,
            NewQueuedMedia {
                account_id,
                media_id,
                file_name: &format!("{media_id}.orig"),
                content_type: "video/webm",
                kind: "video",
                description: None,
                focus: None,
            },
        )
        .await
        .unwrap();
        fail_processing(&pool, media_id).await.unwrap();
        let row = find_owned(&pool, media_id, account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.processing, "failed");
        assert!(row.not_processed());
    }

    #[sqlx::test]
    async fn suspension_privates_and_unsuspension_republishes_stored_media(pool: PgPool) {
        let (account_id, _) = fixture(&pool).await;
        sqlx::query!(
            "UPDATE accounts SET avatar_file_name = 'avatar.png' WHERE id = $1",
            account_id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(public_file_allowed(&pool, "avatar.png").await.unwrap());

        crate::account::suspend(&pool, account_id, "local")
            .await
            .unwrap();
        assert!(!public_file_allowed(&pool, "avatar.png").await.unwrap());

        crate::account::unsuspend(&pool, account_id).await.unwrap();
        assert!(public_file_allowed(&pool, "avatar.png").await.unwrap());
    }
}
