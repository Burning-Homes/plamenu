//! The domain-block severance queue: removing a user's follow edges with a
//! blocked domain, taken off the request that recorded the block.
//!
//! Claims lease jobs rather than deleting them, so a worker that
//! dies mid-severance leaves the row to become due again. The severance itself
//! is set-based and idempotent, so a reclaim redoes only what the previous
//! attempt did not finish.

use sqlx::PgPool;

use crate::{DbError, id};

/// How long a claimed severance stays invisible before a crashed worker's job
/// becomes due again. The work is a handful of set-based statements, so this
/// is generous.
const LEASE_SECONDS: f64 = 300.0;

/// A claimed severance attempt.
#[derive(Debug, Clone)]
pub struct ClaimedSeverance {
    /// The job's own id, to [`complete`] it once the attempt settles.
    pub id: i64,
    pub account_id: i64,
    pub domain: String,
    pub attempts: i32,
}

/// Queues a severance; one already queued for the same (account, domain) wins.
pub async fn enqueue<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    domain: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO domain_severance_jobs (id, account_id, domain)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, domain) DO NOTHING
        "#,
        id::next(),
        account_id,
        domain,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims due severances oldest first, leasing each for [`LEASE_SECONDS`].
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedSeverance>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedSeverance,
        r#"
        UPDATE domain_severance_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM domain_severance_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, account_id, domain, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a settled (or retry-exhausted) severance.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM domain_severance_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Makes a leased severance due again after `delay`, keeping its attempt count.
pub async fn reschedule(pool: &PgPool, id: i64, delay: std::time::Duration) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE domain_severance_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        id,
        delay.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(())
}
