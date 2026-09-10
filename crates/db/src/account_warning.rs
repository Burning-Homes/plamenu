//! Moderator strikes against an account (Mastodon's `AccountWarning`). Every
//! `Admin::AccountAction` records one of these — the audit trail of what a
//! moderator did, with the explanatory text and the optionally-cited report.

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

/// A recorded moderation action against an account.
#[derive(Debug, Clone)]
pub struct AccountWarning {
    pub id: i64,
    /// The acting moderator's account, `None` once that account is gone.
    pub account_id: Option<i64>,
    pub target_account_id: i64,
    /// `none`/`disable`/`sensitive`/`silence`/`suspend`.
    pub action: String,
    pub text: String,
    pub report_id: Option<i64>,
    pub status_ids: Vec<i64>,
    pub created_at: OffsetDateTime,
}

/// The fields of a fresh strike.
#[derive(Debug, Default)]
pub struct NewAccountWarning<'a> {
    pub account_id: Option<i64>,
    pub target_account_id: i64,
    pub action: &'a str,
    pub text: &'a str,
    pub report_id: Option<i64>,
    pub status_ids: &'a [i64],
}

/// Records a moderation strike, returning the stored row.
pub async fn create(pool: &PgPool, new: NewAccountWarning<'_>) -> Result<AccountWarning, DbError> {
    insert(pool, new).await
}

/// Records a strike inside the caller's transaction. Moderation state, its
/// warning, report resolution, and audit line must be one atomic decision.
pub async fn create_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    new: NewAccountWarning<'_>,
) -> Result<AccountWarning, DbError> {
    insert(&mut **tx, new).await
}

async fn insert<'e, E: PgExecutor<'e>>(
    executor: E,
    new: NewAccountWarning<'_>,
) -> Result<AccountWarning, DbError> {
    let warning = sqlx::query_as!(
        AccountWarning,
        r#"
        INSERT INTO account_warnings
            (id, account_id, target_account_id, action, text, report_id, status_ids)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, account_id, target_account_id, action, text, report_id,
                  status_ids, created_at
        "#,
        id::next(),
        new.account_id,
        new.target_account_id,
        new.action,
        new.text,
        new.report_id,
        new.status_ids,
    )
    .fetch_one(executor)
    .await?;
    Ok(warning)
}

pub async fn find_by_id(pool: &PgPool, warning_id: i64) -> Result<Option<AccountWarning>, DbError> {
    let warning = sqlx::query_as!(
        AccountWarning,
        r#"
        SELECT id, account_id, target_account_id, action, text, report_id,
               status_ids, created_at
        FROM account_warnings
        WHERE id = $1
        "#,
        warning_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(warning)
}

/// [`find_by_id`] over a set of strike ids in one query — the notification
/// renderer resolves every distinct `moderation_warning` strike on a page
/// with one round trip. Unknown ids are simply absent.
pub async fn find_by_ids(
    pool: &PgPool,
    warning_ids: &[i64],
) -> Result<Vec<AccountWarning>, DbError> {
    if warning_ids.is_empty() {
        return Ok(Vec::new());
    }
    let warnings = sqlx::query_as!(
        AccountWarning,
        r#"
        SELECT id, account_id, target_account_id, action, text, report_id,
               status_ids, created_at
        FROM account_warnings
        WHERE id = ANY($1)
        "#,
        warning_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(warnings)
}

/// Marks a strike overruled — an approved appeal reversed it (Mastodon
/// touches `overruled_at` in `ApproveAppealService`).
pub async fn overrule(pool: &PgPool, warning_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"UPDATE account_warnings SET overruled_at = now() WHERE id = $1"#,
        warning_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The strikes recorded against an account, newest first.
pub async fn for_target(
    pool: &PgPool,
    target_account_id: i64,
) -> Result<Vec<AccountWarning>, DbError> {
    let warnings = sqlx::query_as!(
        AccountWarning,
        r#"
        SELECT id, account_id, target_account_id, action, text, report_id,
               status_ids, created_at
        FROM account_warnings
        WHERE target_account_id = $1
        ORDER BY id DESC
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(warnings)
}
