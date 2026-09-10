//! Durable fixed-window rate-limit counters: the storage behind
//! the security-sensitive admission buckets, which must survive a process
//! restart and be shared across app instances. One row per (bucket, identity);
//! the window itself is the caller's `period_index` (epoch seconds divided by
//! the bucket period), so a row from a past window is simply overwritten in
//! place rather than needing its own cleanup to stay correct. `expires_at`
//! exists only so the hourly maintenance sweep can drop rows nobody will touch
//! again.

use time::OffsetDateTime;

use crate::{DbError, PgPool};

/// Counts one request against `(bucket, identity)` in the window
/// `period_index` and returns the post-increment count, atomically: concurrent
/// callers serialize on the row, so no request is ever lost between a read and
/// a write. A row left over from an earlier window restarts at 1.
pub async fn increment(
    pool: &PgPool,
    bucket: &str,
    identity: &str,
    period_index: i64,
    expires_at: OffsetDateTime,
) -> Result<i32, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        INSERT INTO rate_limit_windows (bucket, identity, period_index, count, expires_at)
        VALUES ($1, $2, $3, 1, $4)
        ON CONFLICT (bucket, identity) DO UPDATE SET
            count = CASE
                WHEN rate_limit_windows.period_index = excluded.period_index
                    THEN rate_limit_windows.count + 1
                ELSE 1
            END,
            period_index = excluded.period_index,
            expires_at = excluded.expires_at
        RETURNING count
        "#,
        bucket,
        identity,
        period_index,
        expires_at,
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Drops windows whose period has ended — correctness never depends on this
/// (an out-of-window row restarts at 1 on the next increment); it only keeps
/// the table from accumulating one dead row per identity ever throttled.
pub async fn sweep_expired(pool: &PgPool) -> Result<u64, DbError> {
    let result = sqlx::query!("DELETE FROM rate_limit_windows WHERE expires_at < now()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expiry(secs_from_now: i64) -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::seconds(secs_from_now)
    }

    #[sqlx::test]
    async fn increments_within_a_window_and_resets_across_windows(pool: PgPool) {
        let count = increment(&pool, "login_ip", "ip:192.0.2.1", 100, expiry(300))
            .await
            .unwrap();
        assert_eq!(count, 1);
        let count = increment(&pool, "login_ip", "ip:192.0.2.1", 100, expiry(300))
            .await
            .unwrap();
        assert_eq!(count, 2);
        // A different identity and a different bucket each count separately.
        let count = increment(&pool, "login_ip", "ip:192.0.2.2", 100, expiry(300))
            .await
            .unwrap();
        assert_eq!(count, 1);
        let count = increment(&pool, "sign_up_web", "ip:192.0.2.1", 100, expiry(300))
            .await
            .unwrap();
        assert_eq!(count, 1);
        // A new window restarts the counter in place.
        let count = increment(&pool, "login_ip", "ip:192.0.2.1", 101, expiry(600))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    async fn concurrent_increments_are_never_lost(pool: PgPool) {
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    increment(&pool, "login_ip", "ip:198.51.100.1", 7, expiry(300))
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut counts = Vec::new();
        for task in tasks {
            counts.push(task.await.unwrap());
        }
        counts.sort_unstable();
        // Every increment lands exactly once: the post-increment counts are a
        // permutation of 1..=20, not a smaller set with duplicates.
        assert_eq!(counts, (1..=20).collect::<Vec<_>>());
    }

    #[sqlx::test]
    async fn sweep_removes_only_expired_windows(pool: PgPool) {
        increment(&pool, "login_ip", "ip:192.0.2.1", 1, expiry(-10))
            .await
            .unwrap();
        increment(&pool, "login_ip", "ip:192.0.2.2", 2, expiry(300))
            .await
            .unwrap();
        assert_eq!(sweep_expired(&pool).await.unwrap(), 1);
        // The live window's count survived the sweep.
        let count = increment(&pool, "login_ip", "ip:192.0.2.2", 2, expiry(300))
            .await
            .unwrap();
        assert_eq!(count, 2);
    }
}
