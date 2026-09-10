//! Strike appeals (Mastodon's `Appeal`).
//!
//! A user may appeal each moderation strike once, within
//! [`APPEAL_WINDOW_DAYS`] of receiving it (Mastodon's `APPEAL_WINDOW`).
//! Approving is handled by the server layer (it also reverses the strike);
//! this module owns the rows.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// How long after a strike an appeal may still be filed.
pub const APPEAL_WINDOW_DAYS: i64 = 20;

/// Appeal text length cap (Mastodon's `Appeal::TEXT_LENGTH_LIMIT`).
pub const TEXT_LENGTH_LIMIT: usize = 2_000;

#[derive(Debug, Clone)]
pub struct Appeal {
    pub id: i64,
    pub account_id: i64,
    pub account_warning_id: i64,
    pub text: String,
    pub approved_at: Option<OffsetDateTime>,
    pub approved_by_account_id: Option<i64>,
    pub rejected_at: Option<OffsetDateTime>,
    pub rejected_by_account_id: Option<i64>,
    pub created_at: OffsetDateTime,
}

impl Appeal {
    #[must_use]
    pub fn pending(&self) -> bool {
        self.approved_at.is_none() && self.rejected_at.is_none()
    }
}

/// An appeal joined with its strike and appellant for the admin queue.
#[derive(Debug, Clone)]
pub struct AdminAppeal {
    pub appeal: Appeal,
    /// The appealed strike's action verb (`suspend`, `silence`, …).
    pub strike_action: String,
    /// The strike's explanatory text.
    pub strike_text: String,
    /// The appellant's local username.
    pub username: String,
}

/// Files an appeal. Fails on the unique index when the strike was already
/// appealed; window and ownership checks are the caller's.
pub async fn create(
    pool: &PgPool,
    account_id: i64,
    account_warning_id: i64,
    text: &str,
) -> Result<Appeal, DbError> {
    let appeal = sqlx::query_as!(
        Appeal,
        r#"
        INSERT INTO appeals (id, account_id, account_warning_id, text)
        VALUES ($1, $2, $3, $4)
        RETURNING id, account_id, account_warning_id, text,
                  approved_at, approved_by_account_id,
                  rejected_at, rejected_by_account_id, created_at
        "#,
        id::next(),
        account_id,
        account_warning_id,
        text,
    )
    .fetch_one(pool)
    .await?;
    Ok(appeal)
}

pub async fn find_by_id(pool: &PgPool, appeal_id: i64) -> Result<Option<Appeal>, DbError> {
    let appeal = sqlx::query_as!(
        Appeal,
        r#"
        SELECT id, account_id, account_warning_id, text,
               approved_at, approved_by_account_id,
               rejected_at, rejected_by_account_id, created_at
        FROM appeals
        WHERE id = $1
        "#,
        appeal_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(appeal)
}

/// Every appeal filed by `account_id`, newest first — for rendering appeal
/// state next to the user's own strikes.
pub async fn for_account(pool: &PgPool, account_id: i64) -> Result<Vec<Appeal>, DbError> {
    let appeals = sqlx::query_as!(
        Appeal,
        r#"
        SELECT id, account_id, account_warning_id, text,
               approved_at, approved_by_account_id,
               rejected_at, rejected_by_account_id, created_at
        FROM appeals
        WHERE account_id = $1
        ORDER BY id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(appeals)
}

/// The appeal filed against one strike, if any (`appeals.account_warning_id`
/// is unique — one appeal per strike).
pub async fn find_by_warning(pool: &PgPool, warning_id: i64) -> Result<Option<Appeal>, DbError> {
    let appeal = sqlx::query_as!(
        Appeal,
        r#"
        SELECT id, account_id, account_warning_id, text,
               approved_at, approved_by_account_id,
               rejected_at, rejected_by_account_id, created_at
        FROM appeals
        WHERE account_warning_id = $1
        "#,
        warning_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(appeal)
}

/// [`find_by_warning`] over a set of strikes in one query, keyed by
/// `account_warning_id` (unique — one appeal per strike). Strikes with no
/// appeal are simply absent.
pub async fn find_by_warnings(pool: &PgPool, warning_ids: &[i64]) -> Result<Vec<Appeal>, DbError> {
    if warning_ids.is_empty() {
        return Ok(Vec::new());
    }
    let appeals = sqlx::query_as!(
        Appeal,
        r#"
        SELECT id, account_id, account_warning_id, text,
               approved_at, approved_by_account_id,
               rejected_at, rejected_by_account_id, created_at
        FROM appeals
        WHERE account_warning_id = ANY($1)
        "#,
        warning_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(appeals)
}

/// The admin queue: appeals joined with strike and appellant, newest first.
/// `pending_only` narrows to unresolved ones (the default admin view).
pub async fn list_admin(
    pool: &PgPool,
    pending_only: bool,
    limit: i64,
) -> Result<Vec<AdminAppeal>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT ap.id, ap.account_id, ap.account_warning_id, ap.text,
               ap.approved_at, ap.approved_by_account_id,
               ap.rejected_at, ap.rejected_by_account_id, ap.created_at,
               w.action AS strike_action, w.text AS strike_text,
               a.username
        FROM appeals ap
        JOIN account_warnings w ON w.id = ap.account_warning_id
        JOIN accounts a ON a.id = ap.account_id
        WHERE NOT $1::boolean OR (ap.approved_at IS NULL AND ap.rejected_at IS NULL)
        ORDER BY ap.id DESC
        LIMIT $2
        "#,
        pending_only,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AdminAppeal {
            appeal: Appeal {
                id: row.id,
                account_id: row.account_id,
                account_warning_id: row.account_warning_id,
                text: row.text,
                approved_at: row.approved_at,
                approved_by_account_id: row.approved_by_account_id,
                rejected_at: row.rejected_at,
                rejected_by_account_id: row.rejected_by_account_id,
                created_at: row.created_at,
            },
            strike_action: row.strike_action,
            strike_text: row.strike_text,
            username: row.username,
        })
        .collect())
}

/// Stamps a pending appeal approved. Returns `None` when the appeal is
/// unknown or already resolved.
pub async fn approve(
    pool: &PgPool,
    appeal_id: i64,
    moderator_account_id: i64,
) -> Result<Option<Appeal>, DbError> {
    let appeal = sqlx::query_as!(
        Appeal,
        r#"
        UPDATE appeals SET
            approved_at = now(),
            approved_by_account_id = $2
        WHERE id = $1 AND approved_at IS NULL AND rejected_at IS NULL
        RETURNING id, account_id, account_warning_id, text,
                  approved_at, approved_by_account_id,
                  rejected_at, rejected_by_account_id, created_at
        "#,
        appeal_id,
        moderator_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(appeal)
}

/// Stamps a pending appeal rejected. Returns `None` when the appeal is
/// unknown or already resolved.
pub async fn reject(
    pool: &PgPool,
    appeal_id: i64,
    moderator_account_id: i64,
) -> Result<Option<Appeal>, DbError> {
    let appeal = sqlx::query_as!(
        Appeal,
        r#"
        UPDATE appeals SET
            rejected_at = now(),
            rejected_by_account_id = $2
        WHERE id = $1 AND approved_at IS NULL AND rejected_at IS NULL
        RETURNING id, account_id, account_warning_id, text,
                  approved_at, approved_by_account_id,
                  rejected_at, rejected_by_account_id, created_at
        "#,
        appeal_id,
        moderator_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(appeal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::account_warning::{self, NewAccountWarning};

    async fn seed_strike(pool: &PgPool) -> (i64, i64) {
        let target = account::create_local(
            pool,
            NewLocalAccount {
                username: "bob",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let warning = account_warning::create(
            pool,
            NewAccountWarning {
                account_id: None,
                target_account_id: target.id,
                action: "silence",
                text: "tone it down",
                report_id: None,
                status_ids: &[],
            },
        )
        .await
        .unwrap();
        (target.id, warning.id)
    }

    #[sqlx::test]
    async fn appeal_lifecycle_and_single_shot(pool: PgPool) {
        let (account_id, warning_id) = seed_strike(&pool).await;

        let appeal = create(&pool, account_id, warning_id, "I was misread")
            .await
            .unwrap();
        assert!(appeal.pending());

        // One appeal per strike (unique index).
        assert!(
            create(&pool, account_id, warning_id, "again")
                .await
                .is_err()
        );

        let queue = list_admin(&pool, true, 10).await.unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].username, "bob");
        assert_eq!(queue[0].strike_action, "silence");

        let approved = approve(&pool, appeal.id, account_id)
            .await
            .unwrap()
            .unwrap();
        assert!(!approved.pending());
        // Resolving is single-shot.
        assert!(
            reject(&pool, appeal.id, account_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            approve(&pool, appeal.id, account_id)
                .await
                .unwrap()
                .is_none()
        );

        assert!(list_admin(&pool, true, 10).await.unwrap().is_empty());
        assert_eq!(list_admin(&pool, false, 10).await.unwrap().len(), 1);
        assert_eq!(for_account(&pool, account_id).await.unwrap().len(), 1);
    }
}
