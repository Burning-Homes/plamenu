//! Invites — Mastodon's `Invite` model: shareable sign-up codes that
//! bypass closed/approval-gated registrations while they are valid.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Invite {
    pub id: i64,
    pub user_id: i64,
    pub code: String,
    pub expires_at: Option<OffsetDateTime>,
    pub max_uses: Option<i32>,
    pub uses: i32,
    pub comment: String,
    pub created_at: OffsetDateTime,
}

impl Invite {
    /// Mastodon's invite-usability check (minus the creator check, which
    /// [`find_valid_by_code`] folds into its query).
    #[must_use]
    pub fn valid_for_use(&self) -> bool {
        self.max_uses.is_none_or(|max| self.uses < max)
            && self
                .expires_at
                .is_none_or(|at| at > OffsetDateTime::now_utc())
    }
}

pub struct NewInvite<'a> {
    pub user_id: i64,
    pub code: &'a str,
    /// Lifetime in seconds; `None` = never expires.
    pub expires_in: Option<i64>,
    pub max_uses: Option<i32>,
    pub comment: &'a str,
}

pub async fn create(pool: &PgPool, new: NewInvite<'_>) -> Result<Invite, DbError> {
    // Lifetimes are at most weeks, far inside f64's exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    let expires_in_secs = new.expires_in.map(|s| s as f64);
    let invite = sqlx::query_as!(
        Invite,
        r#"
        INSERT INTO invites (id, user_id, code, expires_at, max_uses, comment)
        VALUES ($1, $2, $3, now() + make_interval(secs => $4), $5, $6)
        RETURNING id, user_id, code, expires_at, max_uses, uses, comment, created_at
        "#,
        id::next(),
        new.user_id,
        new.code,
        // make_interval(NULL) is NULL, so `now() + NULL` stores never-expires.
        expires_in_secs,
        new.max_uses,
        new.comment,
    )
    .fetch_one(pool)
    .await?;
    Ok(invite)
}

/// The creator's invites, newest first (the `/invites` management page).
pub async fn list_by_user(pool: &PgPool, user_id: i64) -> Result<Vec<Invite>, DbError> {
    let invites = sqlx::query_as!(
        Invite,
        r#"
        SELECT id, user_id, code, expires_at, max_uses, uses, comment, created_at
        FROM invites
        WHERE user_id = $1
        ORDER BY id DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(invites)
}

/// A code's invite regardless of validity — distinguishes an unknown code
/// (404) from a dead one (401) on `GET /invite/{code}`.
pub async fn find_by_code(pool: &PgPool, code: &str) -> Result<Option<Invite>, DbError> {
    let invite = sqlx::query_as!(
        Invite,
        r#"
        SELECT id, user_id, code, expires_at, max_uses, uses, comment, created_at
        FROM invites
        WHERE code = $1
        "#,
        code,
    )
    .fetch_optional(pool)
    .await?;
    Ok(invite)
}

/// An invite that is currently usable for sign-up: not expired, not used up,
/// and its creator is still a functional, unsuspended login — matching
/// Mastodon's invite-usability rules. Unknown or dead codes are `None`.
pub async fn find_valid_by_code(pool: &PgPool, code: &str) -> Result<Option<Invite>, DbError> {
    let invite = sqlx::query_as!(
        Invite,
        r#"
        SELECT i.id, i.user_id, i.code, i.expires_at, i.max_uses, i.uses,
               i.comment, i.created_at
        FROM invites i
        JOIN users u ON u.id = i.user_id
        JOIN accounts a ON a.id = u.account_id
        WHERE i.code = $1
          AND (i.expires_at IS NULL OR i.expires_at > now())
          AND (i.max_uses IS NULL OR i.uses < i.max_uses)
          AND u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled
          AND a.suspended_at IS NULL
        "#,
        code,
    )
    .fetch_optional(pool)
    .await?;
    Ok(invite)
}

/// One invite in the site-wide admin overview, with the creator's username
/// joined in.
#[derive(Debug, Clone)]
pub struct AdminInvite {
    pub invite: Invite,
    pub username: String,
}

/// Every user's invites, newest first, id-keyset paginated — the admin
/// overview (Mastodon's `admin/invites`).
pub async fn list_all(
    pool: &PgPool,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<AdminInvite>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT i.id, i.user_id, i.code, i.expires_at, i.max_uses, i.uses,
               i.comment, i.created_at, a.username
        FROM invites i
        JOIN users u ON u.id = i.user_id
        JOIN accounts a ON a.id = u.account_id
        WHERE ($1::bigint IS NULL OR i.id < $1)
        ORDER BY i.id DESC
        LIMIT $2
        "#,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AdminInvite {
            invite: Invite {
                id: row.id,
                user_id: row.user_id,
                code: row.code,
                expires_at: row.expires_at,
                max_uses: row.max_uses,
                uses: row.uses,
                comment: row.comment,
                created_at: row.created_at,
            },
            username: row.username,
        })
        .collect())
}

/// Deactivates any user's invite — the admin variant of [`expire`], without
/// the ownership guard. Returns whether a live invite was expired.
pub async fn expire_any(pool: &PgPool, invite_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE invites SET expires_at = now()
        WHERE id = $1 AND (expires_at IS NULL OR expires_at > now())
        "#,
        invite_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deactivates an invite, matching Mastodon's invite expiry: stamps
/// `expires_at` to now. The
/// `user_id` guard keeps users from expiring each other's codes. Returns
/// whether a live invite was expired.
pub async fn expire(pool: &PgPool, invite_id: i64, user_id: i64) -> Result<bool, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE invites SET expires_at = now()
        WHERE id = $1 AND user_id = $2
          AND (expires_at IS NULL OR expires_at > now())
        "#,
        invite_id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::user;

    async fn creator(pool: &PgPool) -> i64 {
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
        user::create(pool, account.id, Some("alice@example.com"), "$argon2id$x")
            .await
            .unwrap()
            .id
    }

    #[sqlx::test]
    async fn lifecycle_and_validity(pool: PgPool) {
        let user_id = creator(&pool).await;
        let invite = create(
            &pool,
            NewInvite {
                user_id,
                code: "AbCd1234",
                expires_in: Some(86_400),
                max_uses: Some(2),
                comment: "for friends",
            },
        )
        .await
        .unwrap();
        assert!(invite.valid_for_use());
        assert!(invite.expires_at.is_some());

        let found = find_valid_by_code(&pool, "AbCd1234")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, invite.id);
        assert!(
            find_valid_by_code(&pool, "unknown")
                .await
                .unwrap()
                .is_none()
        );

        // Used up: max_uses reached kills it.
        sqlx::query!("UPDATE invites SET uses = 2 WHERE id = $1", invite.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            find_valid_by_code(&pool, "AbCd1234")
                .await
                .unwrap()
                .is_none()
        );

        // A never-expiring, unlimited invite; deactivation kills it.
        let forever = create(
            &pool,
            NewInvite {
                user_id,
                code: "Forever1",
                expires_in: None,
                max_uses: None,
                comment: "",
            },
        )
        .await
        .unwrap();
        assert!(forever.expires_at.is_none());
        assert!(
            find_valid_by_code(&pool, "Forever1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(expire(&pool, forever.id, user_id).await.unwrap());
        assert!(
            find_valid_by_code(&pool, "Forever1")
                .await
                .unwrap()
                .is_none()
        );
        // Idempotent-ish: a dead invite reports false.
        assert!(!expire(&pool, forever.id, user_id).await.unwrap());

        let mine = list_by_user(&pool, user_id).await.unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].code, "Forever1"); // newest first
    }

    #[sqlx::test]
    async fn invalid_when_creator_not_functional(pool: PgPool) {
        let user_id = creator(&pool).await;
        create(
            &pool,
            NewInvite {
                user_id,
                code: "Friendly",
                expires_in: None,
                max_uses: None,
                comment: "",
            },
        )
        .await
        .unwrap();
        assert!(
            find_valid_by_code(&pool, "Friendly")
                .await
                .unwrap()
                .is_some()
        );

        let account_id = sqlx::query_scalar!("SELECT account_id FROM users WHERE id = $1", user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        user::set_disabled(&pool, account_id, true).await.unwrap();
        assert!(
            find_valid_by_code(&pool, "Friendly")
                .await
                .unwrap()
                .is_none()
        );
    }
}
