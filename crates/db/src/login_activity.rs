//! The authentication-history log (Mastodon's `login_activities`). Successful
//! sign-ins are inserted alongside the Devise-trackable update in
//! [`crate::user::record_sign_in_with_ip`]; this module owns the failed-attempt
//! rows and the per-user read the security-settings page renders.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// One authentication attempt, successful or not.
#[derive(Debug, Clone)]
pub struct LoginActivity {
    pub id: i64,
    /// `password`, `otp` or `webauthn`; `None` on legacy rows.
    pub authentication_method: Option<String>,
    pub success: bool,
    pub failure_reason: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Records a failed sign-in attempt. Kept separate from the success path (which
/// also advances the trackable columns) — a failure only appends a log row so
/// the user can spot access they don't recognize.
pub async fn record_failure(
    pool: &PgPool,
    user_id: i64,
    method: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
    reason: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO login_activities
             (id, user_id, ip, authentication_method, success, failure_reason, user_agent)
         VALUES ($1, $2, $3, $4, false, $5, $6)",
        id::next(),
        user_id,
        ip,
        method,
        reason,
        user_agent,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether this account has ever successfully signed in from `ip` before
/// — the new-IP alert fires when this is false. Served by
/// `idx_login_activities_user_ip_used`. Call it *before* recording the current
/// sign-in, so the row being added doesn't count itself as familiar.
pub async fn is_familiar_ip(pool: &PgPool, user_id: i64, ip: &str) -> Result<bool, DbError> {
    let seen = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one"
        FROM login_activities
        WHERE user_id = $1 AND ip = $2 AND success
        LIMIT 1
        "#,
        user_id,
        ip,
    )
    .fetch_optional(pool)
    .await?;
    Ok(seen.is_some())
}

/// The user's most recent authentication attempts, newest first, capped at
/// `limit`.
pub async fn list_for_user(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
) -> Result<Vec<LoginActivity>, DbError> {
    let rows = sqlx::query_as!(
        LoginActivity,
        r#"
        SELECT id, authentication_method, success, failure_reason, ip, user_agent,
               created_at
        FROM login_activities
        WHERE user_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        "#,
        user_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::user::{self, SignInContext};

    async fn seed_user(pool: &PgPool) -> i64 {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        user::create(pool, account.id, Some("alice@example.com"), "h")
            .await
            .unwrap()
            .id
    }

    #[sqlx::test]
    async fn failure_then_success_both_listed_newest_first(pool: PgPool) {
        let uid = seed_user(&pool).await;
        record_failure(
            &pool,
            uid,
            "password",
            Some("203.0.113.5"),
            None,
            "invalid_password",
        )
        .await
        .unwrap();
        user::record_sign_in_with_ip(
            &pool,
            uid,
            None,
            Some("203.0.113.6"),
            SignInContext {
                method: Some("password"),
                user_agent: Some("Firefox"),
            },
        )
        .await
        .unwrap();

        let rows = list_for_user(&pool, uid, 20).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first: the successful sign-in leads.
        assert!(rows[0].success);
        assert_eq!(rows[0].user_agent.as_deref(), Some("Firefox"));
        assert!(!rows[1].success);
        assert_eq!(rows[1].failure_reason.as_deref(), Some("invalid_password"));
    }

    #[sqlx::test]
    async fn failed_login_does_not_count_as_active_user(pool: PgPool) {
        let uid = seed_user(&pool).await;
        // Only a failed attempt today — no successful sign-in.
        record_failure(
            &pool,
            uid,
            "password",
            Some("203.0.113.5"),
            None,
            "invalid_password",
        )
        .await
        .unwrap();

        let now = OffsetDateTime::now_utc();
        let measure = crate::metrics::active_users(&pool, now - Duration::days(1), now)
            .await
            .unwrap();
        assert_eq!(measure.total, 0, "a failed attempt is not an active user");

        // A real sign-in does count.
        user::record_sign_in(&pool, uid, None).await.unwrap();
        let after = crate::metrics::active_users(&pool, now - Duration::days(1), now)
            .await
            .unwrap();
        assert_eq!(after.total, 1);
    }
}
