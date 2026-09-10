//! The account-migration replay queue: re-pointing a moved account's local
//! followers, blockers and muters, taken off the request that learned about
//! the move.
//!
//! Claims lease jobs rather than deleting them, so a worker that
//! dies mid-replay leaves the row to become due again. The replay itself is
//! idempotent, so a reclaim redoes only what the previous attempt did not
//! finish.

use sqlx::PgPool;

use crate::{DbError, id};

/// How long a claimed replay stays invisible before a crashed worker's job
/// becomes due again. Generous: one attempt federates a `Follow` and an
/// `Undo(Follow)` per local follower, which is the whole reason this is not on
/// the request path.
const LEASE_SECONDS: f64 = 900.0;

/// A claimed replay attempt.
#[derive(Debug, Clone, Copy)]
pub struct ClaimedMove {
    /// The job's own id, to [`complete`] it once the attempt settles.
    pub id: i64,
    pub source_account_id: i64,
    pub target_account_id: i64,
    pub attempts: i32,
}

/// Queues a move for replay; a replay already queued for the same pair wins.
pub async fn enqueue<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    source: i64,
    target: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO account_move_jobs (id, source_account_id, target_account_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (source_account_id, target_account_id) DO NOTHING
        "#,
        id::next(),
        source,
        target,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims due replays oldest first, leasing each for [`LEASE_SECONDS`].
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedMove>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedMove,
        r#"
        UPDATE account_move_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM account_move_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, source_account_id, target_account_id, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a settled (or retry-exhausted) replay.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM account_move_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Makes a leased replay due again after `delay`, keeping its attempt count.
pub async fn reschedule(pool: &PgPool, id: i64, delay: std::time::Duration) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE account_move_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        id,
        delay.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Replays still queued for a source account — the migration UI's "still
/// working on it" signal, and what the tests assert on.
pub async fn pending_for_source(pool: &PgPool, source: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM account_move_jobs WHERE source_account_id = $1",
        source,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}
