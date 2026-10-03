//! Durable native-video drafts. No encoder is dispatched by this module.
//! Sources are private `.orig` keys. Callers must persist bytes only while
//! holding a current draft/claim fence, and reserve attempt outputs separately.

use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

const LEASE_SECONDS: f64 = 120.0;

#[derive(Debug)]
pub struct Draft {
    pub media_id: i64,
    pub account_id: i64,
    pub channel_id: i64,
    pub title: String,
    pub description: String,
    pub language: Option<String>,
    pub visibility: String,
    pub source_file: String,
    pub source_fingerprint: Option<String>,
    pub poster_time_seconds: f64,
    pub poster_alt: Option<String>,
    pub source_bytes: i64,
    pub reserved_bytes: i64,
    pub state: String,
    pub generation: i64,
    pub lease_until: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
}

pub struct NewDraft<'a> {
    pub media_id: i64,
    pub account_id: i64,
    pub channel_id: i64,
    pub title: &'a str,
    pub description: &'a str,
    pub language: Option<&'a str>,
    pub visibility: &'a str,
    pub source_file: &'a str,
    pub source_fingerprint: Option<&'a str>,
    pub poster_time_seconds: f64,
    pub media_description: Option<&'a str>,
    pub media_transcript: Option<&'a str>,
    pub caption_vtt: Option<&'a str>,
    pub reserved_bytes: i64,
}

/// Source and retained-storage admission, not a substitute for scratch budgets.
pub struct AdmissionLimits {
    pub unfinished_per_account: i64,
    pub retained_source_bytes_per_account: i64,
    pub pending_source_bytes_global: i64,
    pub storage: crate::video_storage::Limits,
    /// A fresh media-filesystem measurement. The admission lock also counts
    /// source reservations that can be written after this measurement.
    pub disk_available_bytes: u64,
    pub min_free_bytes: u64,
    pub processing_reserved_bytes: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Forbidden,
    AccountFull,
    StorageFull,
}

/// Reserve before receiving bytes. The short global lock serializes both
/// aggregates across concurrent accounts; it is never held during an upload.
pub async fn reserve(
    pool: &PgPool,
    new: &NewDraft<'_>,
    limits: &AdmissionLimits,
) -> Result<Admission, DbError> {
    let mut tx = pool.begin().await?;
    let admission = reserve_in_transaction(&mut tx, new, limits).await?;
    if admission == Admission::Accepted {
        tx.commit().await?;
    }
    Ok(admission)
}

/// Compose admission with a durable import/provenance fence. The caller must
/// commit only Accepted work; network or encoder work never runs under it.
pub async fn reserve_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    new: &NewDraft<'_>,
    limits: &AdmissionLimits,
) -> Result<Admission, DbError> {
    crate::video_storage::lock(tx).await?;
    if !crate::video::lock_owned_channel(tx, new.channel_id, new.account_id).await? {
        return Ok(Admission::Forbidden);
    }
    let usage = sqlx::query!(
        r#"SELECT
           (SELECT count(*) FROM video_drafts d JOIN media_attachments m ON m.id = d.media_id
            WHERE d.account_id = $1 AND m.status_id IS NULL) AS "unfinished!",
           (SELECT COALESCE(sum(reserved_bytes), 0)::bigint FROM video_drafts
            WHERE account_id = $1) AS "owned!",
           (SELECT COALESCE(sum(d.reserved_bytes), 0)::bigint FROM media_attachments m
            JOIN video_drafts d ON d.media_id = m.id WHERE m.status_id IS NULL) AS "pending!""#,
        new.account_id,
    )
    .fetch_one(&mut **tx)
    .await?;
    if usage.unfinished >= limits.unfinished_per_account {
        return Ok(Admission::AccountFull);
    }
    if new.reserved_bytes <= 0
        || new.reserved_bytes
            > limits
                .retained_source_bytes_per_account
                .saturating_sub(usage.owned)
        || new.reserved_bytes
            > limits
                .pending_source_bytes_global
                .saturating_sub(usage.pending)
    {
        return Ok(Admission::StorageFull);
    }
    let needed = limits
        .min_free_bytes
        .saturating_add(limits.processing_reserved_bytes)
        .saturating_add(u64::try_from(usage.pending).unwrap_or(u64::MAX))
        .saturating_add(u64::try_from(new.reserved_bytes).unwrap_or(u64::MAX));
    if needed > limits.disk_available_bytes {
        return Ok(Admission::StorageFull);
    }
    if !crate::video_storage::reserve_locked(
        tx,
        new.account_id,
        new.media_id,
        &[new.source_file.to_owned()],
        &[new.reserved_bytes],
        &limits.storage,
    )
    .await?
    {
        return Ok(Admission::StorageFull);
    }
    sqlx::query!(
        "INSERT INTO media_attachments (id, account_id, file_name, content_type, kind, processing,
         description, transcript, caption_vtt)
         VALUES ($1, $2, $3, 'video/mp4', 'video', 'queued', $4, $5, $6)",
        new.media_id,
        new.account_id,
        new.source_file,
        new.media_description,
        new.media_transcript,
        new.caption_vtt,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO video_drafts (media_id, account_id, channel_id, title, description,
         language, visibility, source_file, reserved_bytes, source_cleanup_job_id, source_fingerprint,
         poster_time_seconds)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        new.media_id,
        new.account_id,
        new.channel_id,
        new.title,
        new.description,
        new.language,
        new.visibility,
        new.source_file,
        new.reserved_bytes,
        id::next(),
        new.source_fingerprint,
        new.poster_time_seconds,
    )
    .execute(&mut **tx)
    .await?;
    Ok(Admission::Accepted)
}

pub async fn find_owned<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    media_id: i64,
    owner: i64,
) -> Result<Option<Draft>, DbError> {
    Ok(sqlx::query_as!(
        Draft,
        "SELECT media_id, account_id, channel_id, title, description, language, visibility,
         source_file, source_fingerprint, poster_time_seconds, poster_alt, source_bytes, reserved_bytes, state, generation, lease_until, expires_at
         FROM video_drafts WHERE media_id = $1 AND account_id = $2",
        media_id,
        owner,
    )
    .fetch_optional(executor)
    .await?)
}

/// The creator's recent unpublished drafts, including failures and expired
/// uploads until cleanup removes them. No storage keys are needed for listing.
pub struct Summary {
    pub media_id: i64,
    pub title: String,
}

pub async fn recent_owned(pool: &PgPool, owner: i64) -> Result<Vec<Summary>, DbError> {
    Ok(sqlx::query_as!(
        Summary,
        "SELECT d.media_id, d.title FROM video_drafts d
         JOIN media_attachments m ON m.id = d.media_id
         WHERE d.account_id = $1 AND m.account_id = $1 AND m.status_id IS NULL
         ORDER BY d.media_id DESC LIMIT 100",
        owner,
    )
    .fetch_all(pool)
    .await?)
}

/// Only ready, unexpired, unpublished drafts have an owner preview. Neither
/// knowing a media id nor being the channel account grants access.
pub struct Preview {
    pub file_name: String,
    pub small_file_name: String,
    pub caption_vtt: Option<String>,
}

pub async fn preview(pool: &PgPool, media_id: i64, owner: i64) -> Result<Option<Preview>, DbError> {
    Ok(sqlx::query_as!(Preview,
        r#"SELECT m.file_name AS "file_name!", m.small_file_name AS "small_file_name!", m.caption_vtt
           FROM video_drafts d JOIN media_attachments m ON m.id = d.media_id
           JOIN accounts a ON a.id = d.account_id JOIN accounts c ON c.id = d.channel_id
           WHERE d.media_id = $1 AND d.account_id = $2 AND m.account_id = $2
             AND d.state = 'ready' AND d.expires_at > clock_timestamp()
             AND m.status_id IS NULL AND m.processing = 'complete'
             AND m.file_name IS NOT NULL AND m.file_name <> d.source_file
             AND m.small_file_name IS NOT NULL
             AND a.suspended_at IS NULL AND c.suspended_at IS NULL"#,
        media_id, owner,
    ).fetch_optional(pool).await?)
}

/// Caller holds this media row lock across the short final storage/commit
/// window. Cancellation and purge also delete this row, so cannot race a late
/// source rename. Receiving bodies/encoding must happen outside the transaction.
pub async fn lock_receiving(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE id = $1 AND account_id = $2
         AND status_id IS NULL FOR UPDATE",
        media_id,
        owner,
    )
    .fetch_optional(&mut *conn)
    .await?
    .is_some();
    if !found {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        "SELECT media_id FROM video_drafts WHERE media_id = $1 AND account_id = $2
         AND state = 'receiving' AND expires_at > clock_timestamp() FOR UPDATE",
        media_id,
        owner,
    )
    .fetch_optional(conn)
    .await?
    .is_some())
}

/// Commit only the actual, verified stored length, never a client-reported
/// completion count. Keeping the source key on the draft protects reconciliation.
pub async fn finish_receiving(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
    bytes: i64,
) -> Result<bool, DbError> {
    let changed = sqlx::query!(
        "UPDATE video_drafts SET source_bytes = $3::bigint, reserved_bytes = $3::bigint, state = 'queued'
         WHERE media_id = $1 AND account_id = $2 AND state = 'receiving'
           AND expires_at > clock_timestamp() AND $3::bigint > 0 AND $3::bigint <= reserved_bytes",
        media_id,
        owner,
        bytes,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query!(
            "UPDATE video_storage_reservations r SET reserved_bytes = $2
            FROM video_drafts d WHERE d.media_id = $1 AND r.file_name = d.source_file
            AND r.reserved_bytes >= $2",
            media_id,
            bytes
        )
        .execute(conn)
        .await?;
    }
    Ok(changed)
}

/// Advance a resumable upload only after its bytes have been synced. The
/// caller serializes writers under the media/draft row locks, and storage
/// trims any uncommitted tail back to this offset before the next append.
pub async fn advance_receiving(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
    expected: i64,
    next: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_drafts SET source_bytes = $4
         WHERE media_id = $1 AND account_id = $2 AND state = 'receiving'
           AND source_fingerprint IS NOT NULL AND expires_at > clock_timestamp()
           AND source_bytes = $3 AND $4 > $3 AND $4 <= reserved_bytes",
        media_id,
        owner,
        expected,
        next,
    )
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Claim {
    pub media_id: i64,
    pub generation: i64,
    pub source_file: String,
    pub source_bytes: i64,
    pub poster_time_seconds: f64,
}

/// Size hint before reserving an in-process scratch slot. A second worker can
/// claim this row first; callers must validate the actual claim against their
/// reservation and defer a mismatched job without failing it.
pub async fn next_source_bytes(pool: &PgPool) -> Result<Option<i64>, DbError> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT d.source_bytes FROM video_drafts d
        JOIN media_attachments m ON m.id = d.media_id
        JOIN accounts a ON a.id = d.account_id JOIN accounts c ON c.id = d.channel_id
        LEFT JOIN video_worker_turns t ON t.account_id = d.account_id
        WHERE (d.state = 'queued' OR (d.state = 'processing' AND d.lease_until <= clock_timestamp()))
          AND d.expires_at > clock_timestamp() AND m.status_id IS NULL
          AND a.suspended_at IS NULL AND c.suspended_at IS NULL
        ORDER BY (SELECT count(*) FROM video_drafts active
                  WHERE active.account_id = d.account_id AND active.state = 'processing'
                    AND active.lease_until > clock_timestamp()),
                 t.last_claimed_at NULLS FIRST, d.media_id LIMIT 1")
        .fetch_optional(pool).await?)
}

/// Claim one item per free worker slot. A crashed process leaves its source
/// intact; expiry makes a new generation eligible, fencing the previous worker.
pub async fn claim(pool: &PgPool) -> Result<Option<Claim>, DbError> {
    let mut tx = pool.begin().await?;
    // Serialize only the tiny scheduling decision, never the encode or a file
    // write. Every worker sees the previous turn before choosing another one.
    sqlx::query("SELECT pg_advisory_xact_lock(1448035330, 0)")
        .execute(&mut *tx)
        .await?;
    let claimed = sqlx::query_as::<_, Claim>(
        "UPDATE video_drafts SET state = 'processing', generation = generation + 1,
         lease_until = clock_timestamp() + make_interval(secs => $1)
         WHERE media_id = (
             SELECT d.media_id FROM video_drafts d
             JOIN media_attachments m ON m.id = d.media_id
             JOIN accounts a ON a.id = d.account_id
             JOIN accounts c ON c.id = d.channel_id
             LEFT JOIN video_worker_turns t ON t.account_id = d.account_id
             WHERE (d.state = 'queued' OR (d.state = 'processing' AND d.lease_until <= clock_timestamp()))
               AND d.expires_at > clock_timestamp() AND m.status_id IS NULL
               AND a.suspended_at IS NULL AND c.suspended_at IS NULL
             ORDER BY (SELECT count(*) FROM video_drafts active
                       WHERE active.account_id = d.account_id AND active.state = 'processing'
                         AND active.lease_until > clock_timestamp()),
                      t.last_claimed_at NULLS FIRST, d.media_id
             LIMIT 1 FOR UPDATE OF d SKIP LOCKED
         ) RETURNING media_id, generation, source_file, source_bytes, poster_time_seconds",
    ).bind(LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    if let Some(job) = &claimed {
        sqlx::query("INSERT INTO video_worker_turns (account_id, last_claimed_at)
                    SELECT account_id, clock_timestamp() FROM video_drafts WHERE media_id = $1
                    ON CONFLICT (account_id) DO UPDATE SET last_claimed_at = EXCLUDED.last_claimed_at")
            .bind(job.media_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(claimed)
}

/// A raced size hint never consumes a job or its original. Its generation is
/// still advanced, fencing any stale worker before the next scheduler pass.
pub async fn defer(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_drafts SET state = 'queued', lease_until = NULL
        WHERE media_id = $1 AND generation = $2 AND state = 'processing'
          AND lease_until > clock_timestamp()",
        claim.media_id,
        claim.generation
    )
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// A late renewal must fail even if nobody has reclaimed the expired job yet.
pub async fn renew(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_drafts d SET lease_until = clock_timestamp() + make_interval(secs => $3)
         WHERE media_id = $1 AND generation = $2 AND state = 'processing'
           AND lease_until > clock_timestamp() AND expires_at > clock_timestamp()
           AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id IN (d.account_id, d.channel_id) AND a.suspended_at IS NOT NULL)",
        claim.media_id, claim.generation, LEASE_SECONDS,
    ).execute(pool).await?.rows_affected() == 1)
}

/// Fence the final storage/DB window against cancellation and newer attempts.
pub async fn lock_claim(conn: &mut PgConnection, claim: &Claim) -> Result<bool, DbError> {
    if sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE id = $1 AND status_id IS NULL FOR UPDATE",
        claim.media_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    .is_none()
    {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        "SELECT media_id FROM video_drafts d WHERE media_id = $1 AND generation = $2
         AND state = 'processing' AND lease_until > clock_timestamp()
         AND expires_at > clock_timestamp()
         AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id IN (d.account_id, d.channel_id) AND a.suspended_at IS NOT NULL)
         FOR UPDATE",
        claim.media_id, claim.generation,
    ).fetch_optional(conn).await?.is_some())
}

/// Commit these reservations BEFORE writing bytes. Reacquire `lock_claim`
/// across the short final storage/completion window. Unique keys must never be
/// reused across attempts; a rejected/uncertain commit is not permission to
/// write them. Prior generations are durably cleaned only under this fence.
pub async fn reserve_outputs(
    pool: &PgPool,
    claim: &Claim,
    main: &str,
    small: &str,
    sizes: [i64; 2],
    limits: &crate::video_storage::Limits,
) -> Result<bool, DbError> {
    reserve_outputs_with_hls(pool, claim, main, small, sizes, &[], limits).await
}

/// The HLS bodies share the same immutable attempt namespace, quota decision,
/// and crash journal as the compatibility pair. Scratch generation is finished
/// before this call; no final-store write may begin on a refused reservation.
pub async fn reserve_outputs_with_hls(
    pool: &PgPool,
    claim: &Claim,
    main: &str,
    small: &str,
    sizes: [i64; 2],
    hls: &[(String, i64)],
    limits: &crate::video_storage::Limits,
) -> Result<bool, DbError> {
    let Some(stem) = main.strip_suffix(".draft.mp4") else {
        return Ok(false);
    };
    if stem.is_empty()
        || !stem.bytes().all(|b| b.is_ascii_alphanumeric())
        || small != format!("{stem}.draft.small.avif")
        || hls.iter().any(|(key, size)| {
            *size <= 0
                || (key != &format!("{stem}.draft.storyboard.jpg")
                    && key
                        .strip_prefix(&format!("{stem}.draft.hls"))
                        .and_then(|tail| tail.strip_suffix(".m4s"))
                        .or_else(|| {
                            key.strip_prefix(&format!("{stem}.draft.progressive"))
                                .and_then(|tail| tail.strip_suffix(".mp4"))
                        })
                        .is_none_or(|height| {
                            height.is_empty()
                                || height.starts_with('0')
                                || !height.bytes().all(|byte| byte.is_ascii_digit())
                        }))
        })
    {
        return Ok(false);
    }
    let mut keys = vec![main.to_owned(), small.to_owned()];
    keys.extend(hls.iter().map(|(key, _)| key.clone()));
    keys.sort_unstable();
    if keys.windows(2).any(|pair| pair[0] == pair[1]) {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    crate::video_storage::lock(&mut tx).await?;
    if !lock_claim(&mut tx, claim).await? {
        return Ok(false);
    }
    // Exactly one pair per generation. Retries must claim a new generation,
    // not replace keys that an uncertain earlier storage operation may own.
    if sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM video_draft_outputs WHERE media_id = $1 AND generation = $2)",
        claim.media_id,
        claim.generation
    )
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(false)
    {
        return Ok(false);
    }
    let owner = sqlx::query_scalar!(
        "SELECT account_id FROM video_drafts WHERE media_id = $1",
        claim.media_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let poster = format!("{stem}.draft.poster.jpg");
    let mut output_keys = vec![main.to_owned(), small.to_owned(), poster.clone()];
    output_keys.extend(hls.iter().map(|(key, _)| key.clone()));
    let mut output_sizes = vec![
        sizes[0],
        sizes[1],
        crate::video_storage::COMPAT_POSTER_BYTES,
    ];
    output_sizes.extend(hls.iter().map(|(_, size)| *size));
    if !crate::video_storage::reserve_locked(
        &mut tx,
        owner,
        claim.media_id,
        &output_keys,
        &output_sizes,
        limits,
    )
    .await?
    {
        return Ok(false);
    }
    // Keep retired keys in the journal until media deletion: even an erroneous
    // retry with the same names must not race a queued cleanup of those names.
    let retired = sqlx::query_scalar!(
        "UPDATE video_draft_outputs SET retired = true
        WHERE media_id = $1 AND generation <> $2 AND NOT retired RETURNING file_name",
        claim.media_id,
        claim.generation
    )
    .fetch_all(&mut *tx)
    .await?;
    crate::media_cleanup::enqueue_many(&mut *tx, &retired).await?;
    let cleanup_ids: Vec<i64> = output_keys.iter().map(|_| id::next()).collect();
    sqlx::query!(
        "INSERT INTO video_draft_outputs (file_name, media_id, generation, cleanup_job_id)
         SELECT entry.file_name, $2, $3, entry.cleanup_job_id
         FROM unnest($1::text[], $4::bigint[]) AS entry(file_name, cleanup_job_id)",
        &output_keys,
        claim.media_id,
        claim.generation,
        &cleanup_ids,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Queue old attempts independently of allocating the new output pair. If
/// cleanup required a successful new allocation, a full quota could deadlock
/// retries forever: the abandoned bytes would never be reclaimed.
pub async fn retire_previous(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    if !lock_claim(&mut tx, claim).await? {
        return Ok(false);
    }
    let keys = sqlx::query_scalar!(
        "UPDATE video_draft_outputs SET retired = true
        WHERE media_id = $1 AND generation <> $2 AND NOT retired RETURNING file_name",
        claim.media_id,
        claim.generation
    )
    .fetch_all(&mut *tx)
    .await?;
    crate::media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    Ok(true)
}

/// Media completion and draft readiness must commit in the same transaction.
/// The worker must not delete files on an uncertain commit outcome.
pub async fn mark_ready(conn: &mut PgConnection, claim: &Claim) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_drafts d SET state = 'ready', lease_until = NULL
         WHERE media_id = $1 AND generation = $2 AND state = 'processing'
           AND lease_until > clock_timestamp() AND expires_at > clock_timestamp()
           AND EXISTS (SELECT 1 FROM media_attachments m WHERE m.id = d.media_id
                       AND m.status_id IS NULL AND m.processing = 'complete'
                       AND m.file_name <> d.source_file AND m.small_file_name IS NOT NULL
                       AND EXISTS (SELECT 1 FROM video_draft_outputs o WHERE o.media_id = d.media_id
                           AND o.generation = d.generation AND NOT o.retired AND o.file_name = m.file_name)
                       AND EXISTS (SELECT 1 FROM video_draft_outputs o WHERE o.media_id = d.media_id
                           AND o.generation = d.generation AND NOT o.retired AND o.file_name = m.small_file_name))",
        claim.media_id,
        claim.generation,
    )
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

pub async fn fail(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    if !lock_claim(&mut tx, claim).await? {
        return Ok(false);
    }
    sqlx::query!(
        "UPDATE video_drafts SET state = 'failed', lease_until = NULL WHERE media_id = $1",
        claim.media_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE media_attachments SET processing = 'failed' WHERE id = $1",
        claim.media_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Retry keeps the original and stable media id. It does not dispatch work
/// itself; a fresh claim gets a fresh generation and must mint new output keys.
pub async fn retry(pool: &PgPool, media_id: i64, owner: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    if sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE id = $1 AND account_id = $2 AND status_id IS NULL FOR UPDATE",
        media_id, owner,
    ).fetch_optional(&mut *tx).await?.is_none() { return Ok(false); }
    let changed = sqlx::query!(
        "UPDATE video_drafts SET state = 'queued', generation = generation + 1
         WHERE media_id = $1 AND account_id = $2 AND state = 'failed' AND expires_at > clock_timestamp()
           AND EXISTS (SELECT 1 FROM accounts a WHERE a.id = video_drafts.account_id AND a.suspended_at IS NULL)
           AND EXISTS (SELECT 1 FROM accounts c WHERE c.id = video_drafts.channel_id AND c.suspended_at IS NULL)",
        media_id, owner,
    ).execute(&mut *tx).await?.rows_affected() == 1;
    if changed {
        sqlx::query!(
            "UPDATE media_attachments SET processing = 'queued' WHERE id = $1",
            media_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Cancellation cannot delete a published film; media deletion cascades the
/// draft and atomically enqueues its private original for cleanup.
pub async fn cancel(pool: &PgPool, media_id: i64, owner: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query!(
        "DELETE FROM media_attachments m WHERE id = $1 AND account_id = $2 AND status_id IS NULL
         AND scheduled_status_id IS NULL
         AND EXISTS (SELECT 1 FROM video_drafts d WHERE d.media_id = m.id)
         RETURNING file_name, small_file_name",
        media_id,
        owner,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let changed = removed.is_some();
    if let Some(row) = removed {
        let files = row
            .file_name
            .into_iter()
            .chain(row.small_file_name)
            .collect::<Vec<_>>();
        crate::media_cleanup::enqueue_many(&mut *tx, &files).await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Explicit draft expiry replaces the ordinary attachment orphan vacuum.
/// Published originals are never expired, and a live lease is allowed to drain.
pub async fn expire(pool: &PgPool, limit: i64) -> Result<usize, DbError> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query!(
        "WITH due AS (
             SELECT m.id FROM media_attachments m JOIN video_drafts d ON d.media_id = m.id
             WHERE m.status_id IS NULL AND m.scheduled_status_id IS NULL
               AND d.expires_at <= clock_timestamp()
               AND (d.lease_until IS NULL OR d.lease_until <= clock_timestamp())
             ORDER BY d.expires_at LIMIT $1 FOR UPDATE OF m SKIP LOCKED
         ) DELETE FROM media_attachments m USING due WHERE m.id = due.id
         RETURNING m.file_name, m.small_file_name",
        limit.clamp(0, 100),
    )
    .fetch_all(&mut *tx)
    .await?;
    let count = removed.len();
    let files = removed
        .into_iter()
        .flat_map(|r| r.file_name.into_iter().chain(r.small_file_name))
        .collect::<Vec<_>>();
    crate::media_cleanup::enqueue_many(&mut *tx, &files).await?;
    tx.commit().await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{account, media_cleanup};

    async fn account(pool: &PgPool, name: &str) -> i64 {
        account::create_local(
            pool,
            account::NewLocalAccount {
                username: name,
                display_name: name,
                note: "",
                public_key_pem: "test",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn setup(pool: &PgPool) -> (i64, i64) {
        let owner = account(pool, "owner").await;
        let channel = account(pool, "films").await;
        let mut tx = pool.begin().await.unwrap();
        crate::video::create_channel(&mut tx, channel, owner)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (owner, channel)
    }

    fn draft(media_id: i64, owner: i64, channel: i64, file: &str) -> NewDraft<'_> {
        NewDraft {
            media_id,
            account_id: owner,
            channel_id: channel,
            title: "A film",
            description: "Saved description",
            language: Some("en"),
            visibility: "unlisted",
            source_file: file,
            source_fingerprint: None,
            poster_time_seconds: 0.0,
            media_description: None,
            media_transcript: None,
            caption_vtt: None,
            reserved_bytes: 100,
        }
    }

    fn limits() -> AdmissionLimits {
        AdmissionLimits {
            unfinished_per_account: 2,
            retained_source_bytes_per_account: 200,
            pending_source_bytes_global: 200,
            storage: crate::video_storage::Limits {
                per_account: 100_000_000,
                global: 100_000_000,
            },
            disk_available_bytes: u64::MAX,
            min_free_bytes: 0,
            processing_reserved_bytes: 0,
        }
    }

    async fn reserve_outputs(
        pool: &PgPool,
        claim: &Claim,
        main: &str,
        small: &str,
    ) -> Result<bool, DbError> {
        super::reserve_outputs(pool, claim, main, small, [10, 10], &limits().storage).await
    }

    async fn queued(pool: &PgPool, owner: i64, channel: i64) -> i64 {
        let media_id = id::next();
        assert_eq!(
            reserve(
                pool,
                &draft(media_id, owner, channel, "source.orig"),
                &limits()
            )
            .await
            .unwrap(),
            Admission::Accepted
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_receiving(&mut tx, media_id, owner).await.unwrap());
        assert!(
            !finish_receiving(&mut tx, media_id, owner, 101)
                .await
                .unwrap()
        );
        assert!(
            finish_receiving(&mut tx, media_id, owner, 80)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        media_id
    }

    #[sqlx::test]
    async fn reservations_are_owned_and_capped_under_concurrency(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let stranger = account(&pool, "stranger").await;
        assert_eq!(
            reserve(
                &pool,
                &draft(id::next(), stranger, channel, "wrong.orig"),
                &limits()
            )
            .await
            .unwrap(),
            Admission::Forbidden
        );
        let a = draft(id::next(), owner, channel, "first.orig");
        let b = draft(id::next(), owner, channel, "second.orig");
        let c = draft(id::next(), owner, channel, "third.orig");
        let policy = limits();
        let results = tokio::join!(
            reserve(&pool, &a, &policy),
            reserve(&pool, &b, &policy),
            reserve(&pool, &c, &policy)
        );
        let results = [results.0.unwrap(), results.1.unwrap(), results.2.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == Admission::Accepted)
                .count(),
            2
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == Admission::AccountFull)
                .count(),
            1
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM media_attachments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            rows, 2,
            "a rejected reservation must not leave media behind"
        );
    }

    #[sqlx::test]
    async fn reservation_bytes_include_other_accounts_and_reject_invalid_sizes(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let other = account(&pool, "other").await;
        let other_channel = account(&pool, "otherfilms").await;
        let mut tx = pool.begin().await.unwrap();
        crate::video::create_channel(&mut tx, other_channel, other)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut policy = limits();
        policy.pending_source_bytes_global = 150;
        assert_eq!(
            reserve(&pool, &draft(id::next(), owner, channel, "a.orig"), &policy)
                .await
                .unwrap(),
            Admission::Accepted
        );
        let mut another = draft(id::next(), other, other_channel, "b.orig");
        assert_eq!(
            reserve(&pool, &another, &policy).await.unwrap(),
            Admission::StorageFull
        );
        another.reserved_bytes = 0;
        assert_eq!(
            reserve(&pool, &another, &policy).await.unwrap(),
            Admission::StorageFull
        );
        another.reserved_bytes = 50;
        assert_eq!(
            reserve(&pool, &another, &policy).await.unwrap(),
            Admission::Accepted
        );
        policy.pending_source_bytes_global = 500;
        policy.retained_source_bytes_per_account = 100;
        assert_eq!(
            reserve(&pool, &draft(id::next(), owner, channel, "c.orig"), &policy)
                .await
                .unwrap(),
            Admission::StorageFull,
            "the account cap applies even with global space available"
        );
    }

    async fn fair_queue(pool: &PgPool, owner: i64, channel: i64) -> i64 {
        let media_id = id::next();
        let file = format!("{media_id}.orig");
        let mut policy = limits();
        policy.unfinished_per_account = 8;
        policy.retained_source_bytes_per_account = 800;
        policy.pending_source_bytes_global = 1600;
        assert_eq!(
            reserve(pool, &draft(media_id, owner, channel, &file), &policy)
                .await
                .unwrap(),
            Admission::Accepted
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_receiving(&mut tx, media_id, owner).await.unwrap());
        assert!(
            finish_receiving(&mut tx, media_id, owner, 80)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        media_id
    }

    async fn other_channel(pool: &PgPool) -> (i64, i64) {
        let owner = account(pool, "other").await;
        let channel = account(pool, "otherfilms").await;
        let mut tx = pool.begin().await.unwrap();
        crate::video::create_channel(&mut tx, channel, owner)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        (owner, channel)
    }

    #[sqlx::test]
    async fn bulk_uploads_do_not_monopolize_concurrent_slots_or_later_turns(pool: PgPool) {
        let (a, ac) = setup(&pool).await;
        let (b, bc) = other_channel(&pool).await;
        let a1 = fair_queue(&pool, a, ac).await;
        let a2 = fair_queue(&pool, a, ac).await;
        let b1 = fair_queue(&pool, b, bc).await;
        let b2 = fair_queue(&pool, b, bc).await;
        let (left, right) = tokio::join!(claim(&pool), claim(&pool));
        let mut first = vec![left.unwrap().unwrap(), right.unwrap().unwrap()];
        first.sort_by_key(|job| job.media_id);
        assert_eq!(
            first.iter().map(|job| job.media_id).collect::<Vec<_>>(),
            [a1, b1]
        );
        for job in &first {
            assert!(fail(&pool, job).await.unwrap());
        }
        // With both slots now free, persisted turns break the tie; FIFO is
        // retained inside each uploader even though B was uploaded later.
        assert_eq!(claim(&pool).await.unwrap().unwrap().media_id, a2);
        assert_eq!(claim(&pool).await.unwrap().unwrap().media_id, b2);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM video_worker_turns")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
    }

    #[sqlx::test]
    async fn failed_turn_commit_rolls_back_the_claim_and_skips_suspended_uploaders(pool: PgPool) {
        let (a, ac) = setup(&pool).await;
        let (b, bc) = other_channel(&pool).await;
        let a1 = fair_queue(&pool, a, ac).await;
        let b1 = fair_queue(&pool, b, bc).await;
        sqlx::raw_sql("CREATE FUNCTION reject_video_turn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture rejected turn'; END $$; CREATE TRIGGER reject_video_turn BEFORE INSERT ON video_worker_turns FOR EACH ROW EXECUTE FUNCTION reject_video_turn()")
            .execute(&pool).await.unwrap();
        assert!(claim(&pool).await.is_err());
        assert_eq!(
            find_owned(&pool, a1, a).await.unwrap().unwrap().state,
            "queued"
        );
        sqlx::raw_sql("DROP TRIGGER reject_video_turn ON video_worker_turns")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE accounts SET suspended_at=now() WHERE id=$1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(next_source_bytes(&pool).await.unwrap(), Some(80));
        assert_eq!(claim(&pool).await.unwrap().unwrap().media_id, b1);
    }

    #[sqlx::test]
    async fn retry_and_expired_claims_preserve_source_and_fence_stale_workers(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        assert!(
            find_owned(&pool, media_id, channel)
                .await
                .unwrap()
                .is_none()
        );
        let saved = find_owned(&pool, media_id, owner).await.unwrap().unwrap();
        assert_eq!(saved.title, "A film");
        assert_eq!(saved.description, "Saved description");
        assert_eq!(saved.visibility, "unlisted");
        assert_eq!(saved.reserved_bytes, 80);
        let first = claim(&pool).await.unwrap().unwrap();
        assert!(claim(&pool).await.unwrap().is_none());
        assert!(renew(&pool, &first).await.unwrap());
        sqlx::query("UPDATE video_drafts SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !renew(&pool, &first).await.unwrap(),
            "expired leases cannot revive"
        );
        let second = claim(&pool).await.unwrap().unwrap();
        assert!(second.generation > first.generation);
        assert!(!fail(&pool, &first).await.unwrap());
        let mut tx = pool.begin().await.unwrap();
        assert!(!lock_claim(&mut tx, &first).await.unwrap());
        assert!(!mark_ready(&mut tx, &first).await.unwrap());
        tx.rollback().await.unwrap();
        assert!(fail(&pool, &second).await.unwrap());
        assert!(
            media_cleanup::referenced_keys(&pool)
                .await
                .unwrap()
                .contains(&"source.orig".into())
        );
        assert!(!retry(&pool, media_id, channel).await.unwrap());
        assert!(retry(&pool, media_id, owner).await.unwrap());
        let third = claim(&pool).await.unwrap().unwrap();
        assert!(third.generation > second.generation);
        assert_eq!(third.source_file, first.source_file);
        assert_eq!(third.media_id, first.media_id);
    }

    #[sqlx::test]
    async fn cancellation_and_expiry_queue_sources_and_outputs_but_not_published_media(
        pool: PgPool,
    ) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        let job = claim(&pool).await.unwrap().unwrap();
        assert!(!cancel(&pool, media_id, channel).await.unwrap());
        assert!(
            reserve_outputs(&pool, &job, "film.draft.mp4", "film.draft.small.avif")
                .await
                .unwrap()
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_claim(&mut tx, &job).await.unwrap());
        assert!(
            !mark_ready(&mut tx, &job).await.unwrap(),
            "readiness requires complete output"
        );
        sqlx::query("UPDATE media_attachments SET processing = 'complete', file_name = 'film.draft.mp4', small_file_name = 'film.draft.small.avif' WHERE id = $1")
            .bind(media_id).execute(&mut *tx).await.unwrap();
        assert!(mark_ready(&mut tx, &job).await.unwrap());
        tx.commit().await.unwrap();
        sqlx::query("UPDATE media_attachments SET created_at = now() - interval '2 days'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            crate::media::vacuum_orphans(&pool, std::time::Duration::from_hours(24), 10)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE video_drafts SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let status_id = id::next();
        sqlx::query("INSERT INTO statuses (id, account_id, content, visibility) VALUES ($1, $2, '', 'unlisted')")
            .bind(status_id).bind(owner).execute(&pool).await.unwrap();
        sqlx::query("UPDATE media_attachments SET status_id = $1 WHERE id = $2")
            .bind(status_id)
            .bind(media_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!cancel(&pool, media_id, owner).await.unwrap());
        assert_eq!(expire(&pool, 10).await.unwrap(), 0);
        sqlx::query("UPDATE media_attachments SET status_id = NULL WHERE id = $1")
            .bind(media_id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(expire(&pool, 10).await.unwrap(), 1);
        let mut keys = media_cleanup::claim_due(&pool, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.file_name)
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "film.draft.mp4",
                "film.draft.poster.jpg",
                "film.draft.small.avif",
                "source.orig"
            ]
        );
        assert!(!renew(&pool, &job).await.unwrap());
    }

    #[sqlx::test]
    async fn cancellation_waits_for_the_fenced_storage_window(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        let job = claim(&pool).await.unwrap().unwrap();
        assert!(
            reserve_outputs(&pool, &job, "late.draft.mp4", "late.draft.small.avif")
                .await
                .unwrap()
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_claim(&mut tx, &job).await.unwrap());
        let cancellation_pool = pool.clone();
        let mut cancellation =
            tokio::spawn(async move { cancel(&cancellation_pool, media_id, owner).await.unwrap() });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut cancellation)
                .await
                .is_err()
        );
        sqlx::query("UPDATE media_attachments SET processing = 'complete', file_name = 'late.draft.mp4', small_file_name = 'late.draft.small.avif' WHERE id = $1")
            .bind(media_id).execute(&mut *tx).await.unwrap();
        assert!(mark_ready(&mut tx, &job).await.unwrap());
        tx.commit().await.unwrap();
        assert!(cancellation.await.unwrap());
        let mut keys = media_cleanup::claim_due(&pool, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.file_name)
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "late.draft.mp4",
                "late.draft.poster.jpg",
                "late.draft.small.avif",
                "source.orig"
            ]
        );
    }

    #[sqlx::test]
    async fn attempt_journal_survives_crashes_and_rejects_stale_or_reused_keys(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        let first = claim(&pool).await.unwrap().unwrap();
        assert!(
            !reserve_outputs(&pool, &first, "unsafe.mp4", "unsafe.avif")
                .await
                .unwrap()
        );
        assert!(
            reserve_outputs(&pool, &first, "first.draft.mp4", "first.draft.small.avif")
                .await
                .unwrap()
        );
        assert!(
            !reserve_outputs(&pool, &first, "other.draft.mp4", "other.draft.small.avif")
                .await
                .unwrap()
        );
        assert!(
            media_cleanup::referenced_keys(&pool)
                .await
                .unwrap()
                .contains(&"first.draft.mp4".into())
        );
        sqlx::query("UPDATE video_drafts SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let second = claim(&pool).await.unwrap().unwrap();
        assert!(
            !reserve_outputs(&pool, &first, "stale.draft.mp4", "stale.draft.small.avif")
                .await
                .unwrap()
        );
        assert!(
            reserve_outputs(&pool, &second, "first.draft.mp4", "first.draft.small.avif")
                .await
                .is_err(),
            "old keys cannot become a new attempt's files"
        );
        assert!(
            reserve_outputs(
                &pool,
                &second,
                "second.draft.mp4",
                "second.draft.small.avif"
            )
            .await
            .unwrap()
        );
        let refs = media_cleanup::referenced_keys(&pool).await.unwrap();
        assert!(!refs.contains(&"first.draft.mp4".into()));
        assert!(refs.contains(&"second.draft.mp4".into()));
        let stale = media_cleanup::claim_due(&pool, 10).await.unwrap();
        assert_eq!(stale.len(), 3);
        assert!(stale.iter().all(|j| j.file_name.starts_with("first.")));
        assert!(cancel(&pool, media_id, owner).await.unwrap());
        let cancelled = media_cleanup::claim_due(&pool, 10).await.unwrap();
        assert_eq!(
            cancelled.len(),
            4,
            "cancel cleans original and unwritten second attempt"
        );
        assert!(
            !crate::media::public_file_allowed(&pool, "first.draft.mp4")
                .await
                .unwrap()
        );
        assert!(
            !crate::media::public_file_allowed(&pool, "second.draft.small.avif")
                .await
                .unwrap()
        );
        assert!(
            !reserve_outputs(&pool, &second, "gone.draft.mp4", "gone.draft.small.avif")
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn readiness_requires_this_generations_reserved_outputs(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        let job = claim(&pool).await.unwrap().unwrap();
        assert!(
            reserve_outputs(&pool, &job, "ready.draft.mp4", "ready.draft.small.avif")
                .await
                .unwrap()
        );
        let mut tx = pool.begin().await.unwrap();
        assert!(lock_claim(&mut tx, &job).await.unwrap());
        sqlx::query("UPDATE media_attachments SET processing = 'complete', small_file_name = 'ready.draft.small.avif' WHERE id = $1").bind(media_id).execute(&mut *tx).await.unwrap();
        assert!(
            !mark_ready(&mut tx, &job).await.unwrap(),
            "original is not a rendition"
        );
        sqlx::query("UPDATE media_attachments SET file_name = 'foreign.draft.mp4' WHERE id = $1")
            .bind(media_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            !mark_ready(&mut tx, &job).await.unwrap(),
            "foreign output is not this attempt's rendition"
        );
        sqlx::query("UPDATE media_attachments SET file_name = 'ready.draft.mp4' WHERE id = $1")
            .bind(media_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(mark_ready(&mut tx, &job).await.unwrap());
        tx.commit().await.unwrap();
        assert!(preview(&pool, media_id, owner).await.unwrap().is_some());
        assert!(preview(&pool, media_id, channel).await.unwrap().is_none());
        assert!(
            crate::media::public_access_blocked(&pool, media_id)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn expiry_waits_for_a_lease_and_suspension_stops_renewals(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let media_id = queued(&pool, owner, channel).await;
        let job = claim(&pool).await.unwrap().unwrap();
        sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!renew(&pool, &job).await.unwrap());
        let mut tx = pool.begin().await.unwrap();
        assert!(!lock_claim(&mut tx, &job).await.unwrap());
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE video_drafts SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(expire(&pool, 10).await.unwrap(), 0);
        sqlx::query("UPDATE video_drafts SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(claim(&pool).await.unwrap().is_none());
        assert_eq!(expire(&pool, 10).await.unwrap(), 1);
        assert!(find_owned(&pool, media_id, owner).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn cancelled_sources_keep_their_storage_charge_until_deleted(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        let mut policy = limits();
        policy.storage = crate::video_storage::Limits {
            per_account: 100,
            global: 100,
        };
        let first = id::next();
        assert_eq!(
            reserve(&pool, &draft(first, owner, channel, "first.orig"), &policy)
                .await
                .unwrap(),
            Admission::Accepted
        );
        assert!(cancel(&pool, first, owner).await.unwrap());
        let next = draft(id::next(), owner, channel, "next.orig");
        assert_eq!(
            reserve(&pool, &next, &policy).await.unwrap(),
            Admission::StorageFull
        );
        let jobs = media_cleanup::claim_due(&pool, 10).await.unwrap();
        crate::video_storage::complete_deleted(
            &pool,
            &jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert_eq!(
            reserve(&pool, &next, &policy).await.unwrap(),
            Admission::Accepted
        );
    }

    #[sqlx::test]
    async fn a_full_quota_does_not_prevent_retiring_a_crashed_attempt(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        queued(&pool, owner, channel).await;
        let first = claim(&pool).await.unwrap().unwrap();
        assert!(
            reserve_outputs(&pool, &first, "first.draft.mp4", "first.draft.small.avif")
                .await
                .unwrap()
        );
        let charged: i64 = sqlx::query_scalar(
            "SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let full = crate::video_storage::Limits {
            per_account: charged,
            global: charged,
        };
        sqlx::query("UPDATE video_drafts SET lease_until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let second = claim(&pool).await.unwrap().unwrap();
        assert!(retire_previous(&pool, &second).await.unwrap());
        assert!(
            !super::reserve_outputs(
                &pool,
                &second,
                "second.draft.mp4",
                "second.draft.small.avif",
                [10, 10],
                &full
            )
            .await
            .unwrap()
        );
        let jobs = media_cleanup::claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 3);
        crate::video_storage::complete_deleted(
            &pool,
            &jobs.iter().map(|j| j.id).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert!(
            super::reserve_outputs(
                &pool,
                &second,
                "second.draft.mp4",
                "second.draft.small.avif",
                [10, 10],
                &full
            )
            .await
            .unwrap()
        );
    }

    #[sqlx::test]
    async fn original_accounting_and_cleanup_follow_account_purge(pool: PgPool) {
        let (owner, channel) = setup(&pool).await;
        queued(&pool, owner, channel).await;
        assert_eq!(
            crate::metrics::media_storage_bytes(&pool).await.unwrap(),
            80
        );
        account::purge_local_data(&pool, owner).await.unwrap();
        assert_eq!(crate::metrics::media_storage_bytes(&pool).await.unwrap(), 0);
        assert_eq!(
            media_cleanup::claim_due(&pool, 10).await.unwrap()[0].file_name,
            "source.orig"
        );
    }
}
