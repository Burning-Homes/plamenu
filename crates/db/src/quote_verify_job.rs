//! The quote re-verification queue (the schedule of Mastodon's quote
//! refetch-and-verify job): pending quotes whose authorization stamp or quoted post could
//! not be verified at ingest are retried with backoff instead of staying
//! `pending` forever. Claims lease jobs until the worker completes them;
//! failures reschedule the existing row via [`reschedule`].

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// How long a claimed verification stays invisible before a crashed worker's
/// job becomes due again. One attempt re-fetches the origin's
/// post under the ordinary federation deadline, so this is generous.
const LEASE_SECONDS: f64 = 300.0;

/// A claimed verification attempt.
#[derive(Debug, Clone, Copy)]
pub struct ClaimedJob {
    /// The job's own id, to [`complete`] it once the attempt settles.
    pub id: i64,
    pub quote_id: i64,
    pub attempts: i32,
}

/// Queues a quote for (re-)verification; a job already queued for it wins.
pub async fn enqueue(pool: &PgPool, quote_id: i64, run_at: OffsetDateTime) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO quote_verify_jobs (id, quote_id, run_at)
        VALUES ($1, $2, $3)
        ON CONFLICT (quote_id) DO NOTHING
        "#,
        id::next(),
        quote_id,
        run_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims due jobs oldest first, leasing each for [`LEASE_SECONDS`] rather than
/// deleting it. The row survives until the worker [`complete`]s
/// it (on a settled verification or an exhausted retry budget) or [`reschedule`]s
/// it (a transient failure), so a crash between claim and either outcome lets
/// the lease expire and the verification is retried instead of leaving the quote
/// `pending` forever. `attempts` is owned by [`reschedule`], not the claim, so a
/// crash reclaim does not consume the retry budget.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedJob>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedJob,
        r#"
        UPDATE quote_verify_jobs SET run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT id FROM quote_verify_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, quote_id, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a settled (or retry-exhausted) verification job.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM quote_verify_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Re-queues a verification that failed transiently, after `delay`, keeping
/// the attempt count so the worker eventually gives up.
pub async fn reschedule(
    pool: &PgPool,
    quote_id: i64,
    attempts: i32,
    delay: std::time::Duration,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO quote_verify_jobs (id, quote_id, run_at, attempts)
        VALUES ($1, $2, now() + make_interval(secs => $3), $4)
        ON CONFLICT (quote_id) DO UPDATE
            SET run_at = EXCLUDED.run_at, attempts = EXCLUDED.attempts
        "#,
        id::next(),
        quote_id,
        delay.as_secs_f64(),
        attempts,
    )
    .execute(pool)
    .await?;
    Ok(())
}
