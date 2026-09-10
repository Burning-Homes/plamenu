//! Remote actor avatar/header caching: a small download queue and the setters
//! the worker writes the cached file name back through. Mirrors the
//! attachment pipeline in [`crate::media`], but profile images live in columns
//! on `accounts` rather than their own rows.

use sqlx::PgPool;

use crate::{DbError, id};

/// Which profile image a job (or setter) targets.
pub const AVATAR: &str = "avatar";
pub const HEADER: &str = "header";

/// How long a claimed avatar/header download stays invisible before a crashed
/// worker's job becomes due again.
const LEASE_SECONDS: f64 = 300.0;

/// A claimed avatar/header download job.
#[derive(Debug, Clone)]
pub struct AccountMediaJob {
    /// The job's own id, to [`complete`] it once the download settles.
    pub id: i64,
    pub account_id: i64,
    /// [`AVATAR`] or [`HEADER`].
    pub which: String,
    pub attempts: i32,
}

/// Clears the cached avatar/header copies of every account on `domain`
/// (Mastodon's `clear_account_images!`), returning the
/// freed file names — avatar in the `file_name` slot, header in
/// `small_file_name`. The remote URLs stay, but the block's `reject_media`
/// stops re-downloads. Up to `limit`; call until empty.
pub async fn clear_cached_for_domain(
    pool: &PgPool,
    domain: &str,
    limit: i64,
) -> Result<Vec<crate::media::ClearedFile>, DbError> {
    let cleared = sqlx::query_as!(
        crate::media::ClearedFile,
        r#"
        WITH due AS (
            SELECT id, avatar_file_name, header_file_name
            FROM accounts
            WHERE domain = $1
              AND (avatar_file_name IS NOT NULL OR header_file_name IS NOT NULL)
            LIMIT $2
        )
        UPDATE accounts a
        SET avatar_file_name = NULL, header_file_name = NULL
        FROM due
        WHERE a.id = due.id
        RETURNING due.avatar_file_name AS "file_name?", due.header_file_name AS "small_file_name?"
        "#,
        domain,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(cleared)
}

/// Queues a download of an actor's avatar or header. One pending job per
/// (account, slot): a duplicate is dropped, so re-ingesting an actor while a
/// download is still pending does not pile up jobs.
pub async fn enqueue(pool: &PgPool, account_id: i64, which: &str) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO account_media_jobs (id, account_id, which) VALUES ($1, $2, $3)
         ON CONFLICT (account_id, which) DO NOTHING",
        id::next(),
        account_id,
        which,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims up to `limit` due avatar/header jobs, leasing each for
/// [`LEASE_SECONDS`] rather than deleting it. The row survives
/// until the worker [`complete`]s it (a settled download) or [`reschedule`]s it
/// (a transient failure), so a crash between claim and either outcome lets the
/// lease expire and the download runs again. `attempts` is owned by
/// [`reschedule`], not the claim, so a crash reclaim does not consume the budget.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<AccountMediaJob>, DbError> {
    let jobs = sqlx::query_as!(
        AccountMediaJob,
        r#"
        UPDATE account_media_jobs SET run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT id FROM account_media_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, account_id, which, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a settled (or abandoned) avatar/header download job.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM account_media_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Re-queues a failed download after `delay`, recording the new attempt count.
/// Upserts so it is safe even if the actor was re-ingested (re-enqueued)
/// between the claim and this reschedule.
pub async fn reschedule(
    pool: &PgPool,
    account_id: i64,
    which: &str,
    attempts: i32,
    delay: std::time::Duration,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO account_media_jobs (id, account_id, which, run_at, attempts)
         VALUES ($1, $2, $3, now() + ($4 * interval '1 second'), $5)
         ON CONFLICT (account_id, which)
         DO UPDATE SET run_at = EXCLUDED.run_at, attempts = EXCLUDED.attempts",
        id::next(),
        account_id,
        which,
        delay.as_secs_f64(),
        attempts,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records the locally cached file name for a downloaded avatar or header.
pub async fn set_file_name(
    pool: &PgPool,
    account_id: i64,
    which: &str,
    file_name: &str,
) -> Result<(), DbError> {
    // `which` is our own constant, never user input, so the column is fixed.
    match which {
        HEADER => {
            sqlx::query!(
                "UPDATE accounts SET header_file_name = $2, header_cached_at = now() WHERE id = $1",
                account_id,
                file_name,
            )
            .execute(pool)
            .await?;
        }
        _ => {
            sqlx::query!(
                "UPDATE accounts SET avatar_file_name = $2, avatar_cached_at = now() WHERE id = $1",
                account_id,
                file_name,
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Evicts cached remote avatars and headers older than the retention period,
/// returning the freed file names (rows keep their remote URLs; the media
/// proxy refetches on demand). Local profile images (no remote URL) never
/// evict.
pub async fn evict_cached_images(
    pool: &PgPool,
    retention: std::time::Duration,
    limit: i64,
) -> Result<Vec<String>, DbError> {
    let mut files = sqlx::query_scalar!(
        r#"
        WITH due AS (
            SELECT id, avatar_file_name
            FROM accounts
            WHERE avatar_remote_url IS NOT NULL
              AND avatar_file_name IS NOT NULL
              AND avatar_cached_at IS NOT NULL
              AND avatar_cached_at < now() - ($1 * interval '1 second')
            ORDER BY avatar_cached_at
            LIMIT $2
        )
        UPDATE accounts a
        SET avatar_file_name = NULL, avatar_cached_at = NULL
        FROM due
        WHERE a.id = due.id
        RETURNING due.avatar_file_name AS "avatar_file_name!"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    let headers = sqlx::query_scalar!(
        r#"
        WITH due AS (
            SELECT id, header_file_name
            FROM accounts
            WHERE header_remote_url IS NOT NULL
              AND header_file_name IS NOT NULL
              AND header_cached_at IS NOT NULL
              AND header_cached_at < now() - ($1 * interval '1 second')
            ORDER BY header_cached_at
            LIMIT $2
        )
        UPDATE accounts a
        SET header_file_name = NULL, header_cached_at = NULL
        FROM due
        WHERE a.id = due.id
        RETURNING due.header_file_name AS "header_file_name!"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    files.extend(headers);
    Ok(files)
}
