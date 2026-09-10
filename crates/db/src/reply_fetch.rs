//! On-demand reply fetching queue (Mastodon's fetch-all-replies job):
//! opening a remote thread enqueues its
//! root for a `replies`-collection crawl. `reply_fetch_jobs` is the queue;
//! `status_reply_fetches` is the per-status cooldown clock (Mastodon's
//! `statuses.fetched_replies_at`).

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::{DbError, id};

/// How long a claimed crawl stays invisible before a crashed worker's job
/// becomes due again. The crawl caps its own wall-clock at two
/// minutes, so this comfortably exceeds one attempt.
const LEASE_SECONDS: f64 = 600.0;

/// Drop a crawl once it has been reclaimed by lease expiry this many times —
/// a process that keeps crashing on the same job must not loop forever. The
/// logical crawl is still single-attempt (a failed fetch is not retried); only
/// crash reclaims bump `attempts`.
const MAX_ATTEMPTS: i32 = 5;

/// A leased crawl job: its own id (to [`complete`] it) and the status to crawl.
#[derive(Debug, Clone, Copy)]
pub struct ClaimedCrawl {
    pub id: i64,
    pub status_id: i64,
    /// Times claimed, including this one; > [`MAX_ATTEMPTS`] means give up.
    pub attempts: i32,
}

impl ClaimedCrawl {
    /// Whether this job has outlived too many process crashes to keep retrying.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.attempts > MAX_ATTEMPTS
    }
}

/// Enqueues a status' replies for crawling. Idempotent while a job is already
/// queued for the same status (the `UNIQUE(status_id)` constraint dedups).
pub async fn enqueue(pool: &PgPool, status_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO reply_fetch_jobs (id, status_id) VALUES ($1, $2)
         ON CONFLICT (status_id) DO NOTHING",
        id::next(),
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Claims up to `limit` due crawl jobs, leasing each for [`LEASE_SECONDS`] and
/// bumping its attempt counter. The row survives until the worker
/// [`complete`]s it, so a crash between claim and completion lets the lease
/// expire and the crawl runs again instead of being silently lost.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedCrawl>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedCrawl,
        r#"
        UPDATE reply_fetch_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM reply_fetch_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, status_id, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a finished (or exhausted) crawl job.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM reply_fetch_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a status is due for a replies crawl: never crawled, or last crawled
/// longer ago than `cooldown` (Mastodon's `FETCH_REPLIES_COOLDOWN_MINUTES`).
pub async fn is_due(pool: &PgPool, status_id: i64, cooldown: Duration) -> Result<bool, DbError> {
    let fetched_at = sqlx::query_scalar!(
        "SELECT fetched_at FROM status_reply_fetches WHERE status_id = $1",
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(match fetched_at {
        Some(fetched_at) => OffsetDateTime::now_utc() - fetched_at >= cooldown,
        None => true,
    })
}

/// Records that a status' replies were just crawled, resetting its cooldown.
pub async fn mark_fetched(pool: &PgPool, status_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO status_reply_fetches (status_id, fetched_at) VALUES ($1, now())
         ON CONFLICT (status_id) DO UPDATE SET fetched_at = now()",
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Number of queued crawl jobs (diagnostics / tests).
pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM reply_fetch_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Test hook: backdates a status' last crawl so cooldown paths can be tested.
pub async fn backdate_fetched_at(
    pool: &PgPool,
    status_id: i64,
    fetched_at: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO status_reply_fetches (status_id, fetched_at) VALUES ($1, $2)
         ON CONFLICT (status_id) DO UPDATE SET fetched_at = $2",
        status_id,
        fetched_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

    async fn a_status(pool: &PgPool) -> i64 {
        let account_id = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;
        status::create_local(
            pool,
            NewLocalStatus::new(account_id, "<p>root</p>", "public", None),
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn enqueue_dedups_claim_leases_and_complete_removes(pool: PgPool) {
        let status_id = a_status(&pool).await;

        // Two enqueues collapse to a single queued job.
        enqueue(&pool, status_id).await.unwrap();
        enqueue(&pool, status_id).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        // Claiming leases the job (does not delete it) and returns its id and
        // status, bumping the attempt counter.
        let claimed = claim_due(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].status_id, status_id);
        assert_eq!(claimed[0].attempts, 1);
        assert!(!claimed[0].exhausted());
        // The row still exists (leased), so a crash before completion keeps it.
        assert_eq!(pending_count(&pool).await.unwrap(), 1);
        // Leased: nothing else claimable until the lease expires.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        // Completing the job removes it for good.
        complete(&pool, claimed[0].id).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());
    }

    /// A crash (no completion) lets the lease expire and the crawl reruns, and
    /// after enough reclaims the job is reported exhausted so it can be dropped
    /// instead of looping forever.
    #[sqlx::test]
    async fn expired_lease_resurfaces_and_eventually_exhausts(pool: PgPool) {
        let status_id = a_status(&pool).await;
        enqueue(&pool, status_id).await.unwrap();

        let first = claim_due(&pool, 10).await.unwrap();
        assert_eq!(first[0].attempts, 1);
        // Simulate a crash: never complete, and force the lease to have expired.
        sqlx::query!("UPDATE reply_fetch_jobs SET run_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let second = claim_due(&pool, 10).await.unwrap();
        assert_eq!(second[0].id, first[0].id, "the same job resurfaced");
        assert_eq!(second[0].attempts, 2, "the reclaim bumped attempts");

        // Drive it past the cap and confirm it is reported exhausted.
        for _ in 0..MAX_ATTEMPTS {
            sqlx::query!("UPDATE reply_fetch_jobs SET run_at = now()")
                .execute(&pool)
                .await
                .unwrap();
            let _ = claim_due(&pool, 10).await.unwrap();
        }
        sqlx::query!("UPDATE reply_fetch_jobs SET run_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let exhausted = claim_due(&pool, 10).await.unwrap();
        assert!(exhausted[0].exhausted(), "reclaimed past the cap");
    }

    #[sqlx::test]
    async fn cooldown_gates_refetch(pool: PgPool) {
        let status_id = a_status(&pool).await;
        let cooldown = Duration::minutes(15);

        // Never fetched → due.
        assert!(is_due(&pool, status_id, cooldown).await.unwrap());

        // Just fetched → not due within the cooldown window.
        mark_fetched(&pool, status_id).await.unwrap();
        assert!(!is_due(&pool, status_id, cooldown).await.unwrap());

        // Backdated past the cooldown → due again.
        backdate_fetched_at(
            &pool,
            status_id,
            OffsetDateTime::now_utc() - Duration::minutes(16),
        )
        .await
        .unwrap();
        assert!(is_due(&pool, status_id, cooldown).await.unwrap());
    }
}
