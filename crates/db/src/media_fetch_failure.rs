//! Negative cache for failed on-demand remote-media fetches.
//!
//! The media proxy downloads a remote avatar/header/attachment/emoji/card on
//! the first request that needs it. When the origin is gone (a rotated avatar
//! the remote 404s) or blocks us (a CDN 403ing our IP) that fetch fails — and
//! without a memory of the failure every later timeline render re-tries it,
//! hammering dead origins and stalling the proxy under a client's request
//! storm. This records the last failure per `(kind, target_id)` so the proxy
//! can skip the retry until a cooldown elapses, then try once more. `kind` is
//! the proxy kind string (`avatar`/`header`/`attachment`/`emoji`/`card`).

use sqlx::PgPool;

use crate::DbError;

/// Lifetime attempts for one immutable remote resource. Once exhausted the
/// resource stays suppressed until its URL/row is replaced or an operator
/// explicitly clears the failure.
pub const MAX_ATTEMPTS: i32 = 8;

/// Records (or refreshes) a failed fetch for `(kind, target_id)`, bumping the
/// attempt counter.
pub async fn record(pool: &PgPool, kind: &str, target_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO media_fetch_failures (kind, target_id, failed_at, attempts)
        VALUES ($1, $2, now(), 1)
        ON CONFLICT (kind, target_id) DO UPDATE
            SET failed_at = now(),
                attempts = media_fetch_failures.attempts + 1
        "#,
        kind,
        target_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether `(kind, target_id)` is currently suppressed: inside its backoff
/// window, or permanently after exhausting its finite lifetime budget.
pub async fn is_cooling_down(
    pool: &PgPool,
    kind: &str,
    target_id: i64,
    cooldown: std::time::Duration,
) -> Result<bool, DbError> {
    let cooling = sqlx::query_scalar!(
        r#"
        SELECT attempts >= $4
               OR failed_at + make_interval(
                    secs => greatest(
                        $3,
                        least(
                            21600.0::float8,
                            60.0::float8 * power(4.0::float8, least(attempts - 1, 8)::float8)
                        )
                    )
                  ) > now() AS "cooling!"
        FROM media_fetch_failures
        WHERE kind = $1 AND target_id = $2
        "#,
        kind,
        target_id,
        cooldown.as_secs_f64(),
        MAX_ATTEMPTS,
    )
    .fetch_optional(pool)
    .await?;
    Ok(cooling.unwrap_or(false))
}

/// Whether the lifetime budget is exhausted, independent of the current
/// backoff window. Queue workers use this to discard a stale re-enqueue
/// without disrupting their own shorter between-attempt schedule.
pub async fn is_abandoned(pool: &PgPool, kind: &str, target_id: i64) -> Result<bool, DbError> {
    let abandoned = sqlx::query_scalar!(
        r#"SELECT attempts >= $3 AS "abandoned!"
           FROM media_fetch_failures
           WHERE kind = $1 AND target_id = $2"#,
        kind,
        target_id,
        MAX_ATTEMPTS,
    )
    .fetch_optional(pool)
    .await?;
    Ok(abandoned.unwrap_or(false))
}

/// Clears any recorded failure for `(kind, target_id)` — called when a later
/// fetch succeeds so the entry does not linger and suppress future refreshes.
pub async fn clear(pool: &PgPool, kind: &str, target_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM media_fetch_failures WHERE kind = $1 AND target_id = $2",
        kind,
        target_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
// `from_secs` keeps these off the nightly-only `duration_constructors` feature
// the pedantic `duration_suboptimal_units` lint would otherwise push us toward.
#[allow(clippy::duration_suboptimal_units)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../db/migrations")]
    async fn records_then_reports_cooling(pool: PgPool) {
        let day = std::time::Duration::from_secs(86_400);
        assert!(!is_cooling_down(&pool, "avatar", 1, day).await.unwrap());

        record(&pool, "avatar", 1).await.unwrap();
        assert!(is_cooling_down(&pool, "avatar", 1, day).await.unwrap());
        // A different kind for the same id is tracked independently.
        assert!(!is_cooling_down(&pool, "header", 1, day).await.unwrap());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn exponential_backoff_applies_even_with_zero_caller_cooldown(pool: PgPool) {
        record(&pool, "card", 7).await.unwrap();
        assert!(
            is_cooling_down(&pool, "card", 7, std::time::Duration::ZERO)
                .await
                .unwrap()
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn record_bumps_attempts_and_clear_removes(pool: PgPool) {
        record(&pool, "emoji", 3).await.unwrap();
        record(&pool, "emoji", 3).await.unwrap();
        let attempts = sqlx::query_scalar!(
            "SELECT attempts FROM media_fetch_failures WHERE kind = 'emoji' AND target_id = 3"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attempts, 2);

        clear(&pool, "emoji", 3).await.unwrap();
        let day = std::time::Duration::from_secs(86_400);
        assert!(!is_cooling_down(&pool, "emoji", 3, day).await.unwrap());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn exhausted_resource_never_automatically_retries(pool: PgPool) {
        for _ in 0..MAX_ATTEMPTS {
            record(&pool, "attachment", 9).await.unwrap();
        }
        sqlx::query!("UPDATE media_fetch_failures SET failed_at = now() - interval '10 years'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            is_cooling_down(&pool, "attachment", 9, std::time::Duration::ZERO)
                .await
                .unwrap()
        );
        assert!(is_abandoned(&pool, "attachment", 9).await.unwrap());
    }
}
