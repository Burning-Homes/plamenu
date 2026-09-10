//! rel="me" link-verification queue — Mastodon's link-verification
//! job. Saving a local profile enqueues its account;
//! the worker fetches each URL-valued profile field and stamps the field's
//! `verified_at` when the target page links back with `rel="me"`.
//!
//! `link_verification_jobs` is the queue, leased-until-complete like
//! `reply_fetch_jobs`. The verification result lives in `account_fields`, so
//! there is no separate result table.

use sqlx::PgPool;

use crate::{DbError, id};

/// How long a claimed verification stays invisible before a crashed worker's
/// job becomes due again.
const LEASE_SECONDS: f64 = 600.0;

/// Drop a verification once it has been reclaimed by lease expiry this many
/// times — a process that keeps crashing on the same job must not loop forever.
/// Verification is otherwise single-attempt; only crash reclaims bump
/// `attempts`.
const MAX_ATTEMPTS: i32 = 5;

/// A leased verification job: its own id (to [`complete`] it) and the account.
#[derive(Debug, Clone, Copy)]
pub struct ClaimedJob {
    pub id: i64,
    pub account_id: i64,
    /// Times claimed, including this one; > [`MAX_ATTEMPTS`] means give up.
    pub attempts: i32,
}

impl ClaimedJob {
    /// Whether this job has outlived too many process crashes to keep retrying.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.attempts > MAX_ATTEMPTS
    }
}

/// Enqueues an account for link verification. Idempotent while a job is already
/// queued for the same account (the `UNIQUE(account_id)` constraint dedups).
pub async fn enqueue<'e, E: sqlx::PgExecutor<'e>>(pool: E, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO link_verification_jobs (id, account_id) VALUES ($1, $2)
         ON CONFLICT (account_id) DO NOTHING",
        id::next(),
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Enqueues an account for link verification after a delay — remote-actor
/// ingest spreads its re-checks out (Mastodon's `rand(VERIFY_DELAY)`). An
/// already queued job keeps the earlier of the two run times.
pub async fn enqueue_in(pool: &PgPool, account_id: i64, delay_seconds: i32) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO link_verification_jobs (id, account_id, run_at)
         VALUES ($1, $2, now() + $3 * interval '1 second')
         ON CONFLICT (account_id) DO UPDATE
         SET run_at = least(link_verification_jobs.run_at, EXCLUDED.run_at)",
        id::next(),
        account_id,
        f64::from(delay_seconds),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims up to `limit` due verification jobs, leasing each for
/// [`LEASE_SECONDS`] and bumping its attempt counter. The row
/// survives until the worker [`complete`]s it, so a crash between claim and
/// completion lets the lease expire and the verification runs again instead of
/// being silently lost.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedJob>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedJob,
        r#"
        UPDATE link_verification_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM link_verification_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, account_id, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a finished (or exhausted) verification job.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM link_verification_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Number of queued verification jobs (diagnostics / tests).
pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM link_verification_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn enqueue_is_idempotent_claim_leases_and_complete_removes(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = local(&pool, "bob").await;

        enqueue(&pool, alice).await.unwrap();
        // A repeat enqueue while queued is a no-op (UNIQUE dedup).
        enqueue(&pool, alice).await.unwrap();
        enqueue(&pool, bob).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 2);

        let claimed = claim_due(&pool, 10).await.unwrap();
        let mut ids: Vec<i64> = claimed.iter().map(|j| j.account_id).collect();
        ids.sort_unstable();
        let mut expected = [alice, bob];
        expected.sort_unstable();
        assert_eq!(ids, expected);
        assert!(claimed.iter().all(|j| j.attempts == 1 && !j.exhausted()));

        // Claiming leased the jobs (did not delete them): the rows survive a
        // crash, and are not re-claimable until the lease expires.
        assert_eq!(pending_count(&pool).await.unwrap(), 2);
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        // Completing each job removes it for good.
        for job in &claimed {
            complete(&pool, job.id).await.unwrap();
        }
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }
}
