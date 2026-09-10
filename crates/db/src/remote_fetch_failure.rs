//! Persistent finite retry budgets for outbound remote fetches.
//!
//! Both a resource key and (for transport/5xx failures) a host key are used by
//! the server transport. The resource key stops one permanently malformed or
//! deleted object; the host key stops a vanished server across all its URLs.

use sqlx::PgPool;

use crate::DbError;

pub const MAX_ATTEMPTS: i32 = 8;

fn backoff_seconds(attempts: i32) -> f64 {
    let exp = u32::try_from(attempts.clamp(1, 9)).unwrap_or(1) - 1;
    f64::from((60u32 * 4u32.saturating_pow(exp)).min(21_600))
}

/// Whether this key may be attempted now. `false` means it is either in its
/// persisted backoff window or has exhausted its lifetime budget.
pub async fn should_attempt(pool: &PgPool, scope: &str, key: &str) -> Result<bool, DbError> {
    let allowed = sqlx::query_scalar!(
        r#"SELECT abandoned_at IS NULL AND (retry_at IS NULL OR retry_at <= now()) AS "allowed!"
           FROM remote_fetch_failures
           WHERE scope = $1 AND failure_key = $2"#,
        scope,
        key,
    )
    .fetch_optional(pool)
    .await?;
    Ok(allowed.unwrap_or(true))
}

/// Records a failed attempt, applying exponential backoff and permanently
/// abandoning the key when its finite budget is exhausted.
pub async fn record_failure(
    pool: &PgPool,
    scope: &str,
    key: &str,
    error: &str,
) -> Result<(), DbError> {
    let next_delay = backoff_seconds(1);
    sqlx::query!(
        r#"INSERT INTO remote_fetch_failures
               (scope, failure_key, attempts, retry_at, last_error)
           VALUES ($1, $2, 1, now() + make_interval(secs => $4), $3)
           ON CONFLICT (scope, failure_key) DO UPDATE SET
               attempts = remote_fetch_failures.attempts + 1,
               retry_at = CASE
                   WHEN remote_fetch_failures.attempts + 1 >= $5 THEN NULL
                   ELSE now() + make_interval(
                       secs => least(21600.0, 60.0 * power(4.0, least(remote_fetch_failures.attempts, 8)))
                   )
               END,
               abandoned_at = CASE
                   WHEN remote_fetch_failures.attempts + 1 >= $5
                       THEN COALESCE(remote_fetch_failures.abandoned_at, now())
                   ELSE remote_fetch_failures.abandoned_at
               END,
               last_error = $3,
               last_failed_at = now()"#,
        scope,
        key,
        error,
        next_delay,
        MAX_ATTEMPTS,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Persists an origin-requested backoff without consuming the finite failure
/// budget. HTTP 429 is pacing, not evidence that the resource is dead.
pub async fn record_backoff(
    pool: &PgPool,
    scope: &str,
    key: &str,
    retry_after_secs: u64,
    error: &str,
) -> Result<(), DbError> {
    let seconds = f64::from(u32::try_from(retry_after_secs.clamp(1, 86_400)).unwrap_or(86_400));
    sqlx::query!(
        r#"INSERT INTO remote_fetch_failures
               (scope, failure_key, attempts, retry_at, last_error)
           VALUES ($1, $2, 0, now() + make_interval(secs => $4), $3)
           ON CONFLICT (scope, failure_key) DO UPDATE SET
               retry_at = GREATEST(
                   COALESCE(remote_fetch_failures.retry_at, '-infinity'::timestamptz),
                   now() + make_interval(secs => $4)
               ),
               last_error = $3,
               last_failed_at = now()"#,
        scope,
        key,
        error,
        seconds,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A successful fetch proves both its resource and host keys live again.
pub async fn clear(pool: &PgPool, scope: &str, key: &str) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM remote_fetch_failures WHERE scope = $1 AND failure_key = $2",
        scope,
        key,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn failure_budget_becomes_terminal_and_success_clears(pool: PgPool) {
        for attempt in 1..=MAX_ATTEMPTS {
            record_failure(&pool, "resource", "https://dead.example/actor", "gone")
                .await
                .unwrap();
            if attempt < MAX_ATTEMPTS {
                sqlx::query!(
                    "UPDATE remote_fetch_failures SET retry_at = now() WHERE scope = 'resource'"
                )
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        assert!(
            !should_attempt(&pool, "resource", "https://dead.example/actor")
                .await
                .unwrap()
        );
        clear(&pool, "resource", "https://dead.example/actor")
            .await
            .unwrap();
        assert!(
            should_attempt(&pool, "resource", "https://dead.example/actor")
                .await
                .unwrap()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn retry_after_backoff_is_durable_without_consuming_attempts(pool: PgPool) {
        let key = "https://paced.example/outbox";
        record_backoff(&pool, "resource", key, 120, "remote answered 429")
            .await
            .unwrap();
        assert!(!should_attempt(&pool, "resource", key).await.unwrap());
        let row = sqlx::query!(
            "SELECT attempts, retry_at FROM remote_fetch_failures
             WHERE scope = 'resource' AND failure_key = $1",
            key,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.attempts, 0, "pacing is not a liveness failure");
        assert!(
            row.retry_at
                .is_some_and(|at| at > time::OffsetDateTime::now_utc())
        );

        // A fresh connection observes the same persisted gate, which is the
        // process-restart contract the federation wrapper relies on.
        let mut connection = pool.acquire().await.unwrap();
        let allowed = sqlx::query_scalar!(
            r#"SELECT (retry_at IS NULL OR retry_at <= now()) AS "allowed!"
               FROM remote_fetch_failures
               WHERE scope = 'resource' AND failure_key = $1"#,
            key,
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap();
        assert!(!allowed);
    }
}
