//! Per-host delivery reachability — the state behind the delivery worker's
//! circuit breaker (migration 0136).
//!
//! Semantics live here so every caller agrees on them:
//! * only **transient** failures (network errors, timeouts, 5xx) feed the failure streak — a host
//!   answering 403/404/410/429 is alive, just unwilling, and must never be branded unreachable;
//! * the breaker opens (`unreachable_since` set) once a host has failed
//!   [`UNREACHABLE_MIN_FAILURES`] consecutive times **and** the streak has lasted
//!   [`UNREACHABLE_MIN_STREAK_HOURS`] — a burst inside one outage window is not enough;
//! * one successful delivery closes the breaker and resets the streak.

use time::OffsetDateTime;

use crate::{DbError, PgPool};

/// Failure classes recorded on the row (`last_failure_class`).
pub const CLASS_GONE: &str = "gone";
pub const CLASS_PERMANENT: &str = "permanent";
pub const CLASS_RATE_LIMITED: &str = "rate_limited";
pub const CLASS_TRANSIENT: &str = "transient";

/// Consecutive transient failures needed before the breaker can open.
pub const UNREACHABLE_MIN_FAILURES: i32 = 10;
/// How long the failure streak must have lasted before the breaker opens.
pub const UNREACHABLE_MIN_STREAK_HOURS: i64 = 24;
/// Once open, this many additional transient failures permanently abandon the
/// host. No timer or probe clears this state; only externally proven success
/// does (for example, a valid fetch initiated by new inbound traffic).
pub const ABANDON_AFTER_FAILURES: i32 = UNREACHABLE_MIN_FAILURES + 8;

#[derive(Debug, Clone)]
pub struct HostReachability {
    pub host: String,
    pub consecutive_failures: i32,
    pub first_failure_at: Option<OffsetDateTime>,
    pub last_failure_at: Option<OffsetDateTime>,
    pub last_failure_class: Option<String>,
    pub last_error: Option<String>,
    pub unreachable_since: Option<OffsetDateTime>,
    pub last_success_at: Option<OffsetDateTime>,
    pub next_probe_at: Option<OffsetDateTime>,
    pub abandoned_at: Option<OffsetDateTime>,
}

pub async fn find(pool: &PgPool, host: &str) -> Result<Option<HostReachability>, DbError> {
    let row = sqlx::query_as!(
        HostReachability,
        r#"SELECT host, consecutive_failures, first_failure_at, last_failure_at,
                  last_failure_class, last_error, unreachable_since,
                  last_success_at, next_probe_at, abandoned_at
           FROM host_reachability WHERE host = $1"#,
        host,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Records a classified delivery failure and returns the updated row (so the
/// caller can log a breaker transition). Only [`CLASS_TRANSIENT`] failures
/// advance the streak or open the breaker; the other classes just update the
/// diagnostic columns.
pub async fn record_failure(
    pool: &PgPool,
    host: &str,
    class: &str,
    error: &str,
) -> Result<HostReachability, DbError> {
    let counts = class == CLASS_TRANSIENT;
    let streak_cutoff =
        OffsetDateTime::now_utc() - time::Duration::hours(UNREACHABLE_MIN_STREAK_HOURS);
    let row = sqlx::query_as!(
        HostReachability,
        r#"INSERT INTO host_reachability
               (host, consecutive_failures, first_failure_at, last_failure_at,
                last_failure_class, last_error)
           VALUES ($1, CASE WHEN $2 THEN 1 ELSE 0 END,
                   CASE WHEN $2 THEN now() END, now(), $3, $4)
           ON CONFLICT (host) DO UPDATE SET
               consecutive_failures = CASE WHEN $2
                   THEN host_reachability.consecutive_failures + 1
                   ELSE host_reachability.consecutive_failures END,
               first_failure_at = CASE WHEN $2
                   THEN COALESCE(host_reachability.first_failure_at, now())
                   ELSE host_reachability.first_failure_at END,
               last_failure_at = now(),
               last_failure_class = $3,
               last_error = $4,
               unreachable_since = CASE
                   WHEN host_reachability.unreachable_since IS NOT NULL
                       THEN host_reachability.unreachable_since
                   WHEN $2
                        AND host_reachability.consecutive_failures + 1 >= $5
                        AND host_reachability.first_failure_at <= $6
                       THEN now()
               END,
               abandoned_at = CASE
                   WHEN host_reachability.abandoned_at IS NOT NULL
                       THEN host_reachability.abandoned_at
                   WHEN $2
                        AND host_reachability.unreachable_since IS NOT NULL
                        AND host_reachability.consecutive_failures + 1 >= $7
                       THEN now()
               END,
               updated_at = now()
           RETURNING host, consecutive_failures, first_failure_at,
                     last_failure_at, last_failure_class, last_error,
                     unreachable_since, last_success_at, next_probe_at, abandoned_at"#,
        host,
        counts,
        class,
        error,
        UNREACHABLE_MIN_FAILURES,
        streak_cutoff,
        ABANDON_AFTER_FAILURES,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Records a successful delivery: closes the breaker and resets the failure
/// streak. Healthy hosts get no row at all, and an existing clean row is
/// refreshed at most hourly, so the steady-state cost is near zero writes.
pub async fn record_success(pool: &PgPool, host: &str) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE host_reachability SET
               consecutive_failures = 0,
               first_failure_at = NULL,
               unreachable_since = NULL,
               next_probe_at = NULL,
               abandoned_at = NULL,
               last_success_at = now(),
               updated_at = now()
           WHERE host = $1
             AND (consecutive_failures > 0
                  OR unreachable_since IS NOT NULL
                  OR next_probe_at IS NOT NULL
                  OR abandoned_at IS NOT NULL
                  OR last_success_at IS NULL
                  OR last_success_at < now() - interval '1 hour')"#,
        host,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Atomically claims the next probe slot for an open-breaker host: succeeds
/// (and pushes `next_probe_at` forward by `interval_secs`) only when no probe
/// is currently pending. The caller lets the claimed delivery through as the
/// probe and skips the rest.
pub async fn try_claim_probe(
    pool: &PgPool,
    host: &str,
    interval_secs: f64,
) -> Result<bool, DbError> {
    let claimed = sqlx::query_scalar!(
        r#"UPDATE host_reachability
           SET next_probe_at = now() + make_interval(secs => $2), updated_at = now()
           WHERE host = $1
             AND unreachable_since IS NOT NULL
             AND abandoned_at IS NULL
             AND (next_probe_at IS NULL OR next_probe_at <= now())
           RETURNING 1 AS "one!""#,
        host,
        interval_secs,
    )
    .fetch_optional(pool)
    .await?;
    Ok(claimed.is_some())
}

/// The hosts currently marked unreachable (diagnostics / operator surface).
pub async fn unreachable_hosts(pool: &PgPool) -> Result<Vec<HostReachability>, DbError> {
    let rows = sqlx::query_as!(
        HostReachability,
        r#"SELECT host, consecutive_failures, first_failure_at, last_failure_at,
                  last_failure_class, last_error, unreachable_since,
                  last_success_at, next_probe_at, abandoned_at
           FROM host_reachability
           WHERE unreachable_since IS NOT NULL
           ORDER BY unreachable_since"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn transient_failures_open_the_breaker_only_after_a_long_streak(pool: PgPool) {
        // Nine transient failures: under the counter threshold, closed.
        for _ in 0..UNREACHABLE_MIN_FAILURES - 1 {
            let row = record_failure(&pool, "down.example", CLASS_TRANSIENT, "timeout")
                .await
                .unwrap();
            assert!(row.unreachable_since.is_none());
        }
        // Tenth failure crosses the counter, but the streak is minutes old —
        // still closed.
        let row = record_failure(&pool, "down.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        assert_eq!(row.consecutive_failures, UNREACHABLE_MIN_FAILURES);
        assert!(row.unreachable_since.is_none());

        // Age the streak past the window; the next failure opens the breaker.
        sqlx::query!(
            "UPDATE host_reachability SET first_failure_at = now() - interval '25 hours'
             WHERE host = 'down.example'"
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = record_failure(&pool, "down.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        assert!(row.unreachable_since.is_some());

        // The stamp is stable across further failures.
        let stamp = row.unreachable_since;
        let row = record_failure(&pool, "down.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        assert_eq!(row.unreachable_since, stamp);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn open_breaker_eventually_abandons_until_proven_success(pool: PgPool) {
        sqlx::query!(
            "INSERT INTO host_reachability
                 (host, consecutive_failures, first_failure_at, unreachable_since)
             VALUES ('gone.example', $1, now() - interval '4 days', now() - interval '2 days')",
            ABANDON_AFTER_FAILURES - 1,
        )
        .execute(&pool)
        .await
        .unwrap();
        let row = record_failure(&pool, "gone.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        assert!(row.abandoned_at.is_some());
        assert!(!try_claim_probe(&pool, "gone.example", 1.0).await.unwrap());

        record_success(&pool, "gone.example").await.unwrap();
        let row = find(&pool, "gone.example").await.unwrap().unwrap();
        assert!(row.abandoned_at.is_none());
        assert!(row.unreachable_since.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn alive_but_unwilling_hosts_never_open_the_breaker(pool: PgPool) {
        for class in [CLASS_GONE, CLASS_PERMANENT, CLASS_RATE_LIMITED] {
            for _ in 0..UNREACHABLE_MIN_FAILURES + 2 {
                let row = record_failure(&pool, "alive.example", class, "answered")
                    .await
                    .unwrap();
                assert_eq!(row.consecutive_failures, 0, "{class}");
                assert!(row.unreachable_since.is_none(), "{class}");
            }
        }
        // The diagnostic columns still record what happened.
        let row = find(&pool, "alive.example").await.unwrap().unwrap();
        assert_eq!(row.last_failure_class.as_deref(), Some(CLASS_RATE_LIMITED));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn success_closes_the_breaker_and_resets_the_streak(pool: PgPool) {
        record_failure(&pool, "flaky.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE host_reachability SET unreachable_since = now(),
                 next_probe_at = now() + interval '6 hours',
                 consecutive_failures = 20
             WHERE host = 'flaky.example'"
        )
        .execute(&pool)
        .await
        .unwrap();

        record_success(&pool, "flaky.example").await.unwrap();
        let row = find(&pool, "flaky.example").await.unwrap().unwrap();
        assert_eq!(row.consecutive_failures, 0);
        assert!(row.unreachable_since.is_none());
        assert!(row.next_probe_at.is_none());
        assert!(row.first_failure_at.is_none());
        assert!(row.last_success_at.is_some());

        // Success on a host with no row stays a no-op (no row minted).
        record_success(&pool, "healthy.example").await.unwrap();
        assert!(find(&pool, "healthy.example").await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn probe_slot_is_claimed_once_per_interval(pool: PgPool) {
        record_failure(&pool, "down.example", CLASS_TRANSIENT, "timeout")
            .await
            .unwrap();
        // Closed breaker: no probes to claim (normal deliveries flow anyway).
        assert!(
            !try_claim_probe(&pool, "down.example", 3600.0)
                .await
                .unwrap()
        );

        sqlx::query!(
            "UPDATE host_reachability SET unreachable_since = now() WHERE host = 'down.example'"
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            try_claim_probe(&pool, "down.example", 3600.0)
                .await
                .unwrap()
        );
        // Second claim inside the interval loses.
        assert!(
            !try_claim_probe(&pool, "down.example", 3600.0)
                .await
                .unwrap()
        );
    }
}
