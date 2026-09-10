//! Outbound account migrations — Mastodon's `account_migrations`. One row per
//! `Move` a local account initiated; the newest row's age drives the
//! between-moves cooldown and the settings page shows the full history.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// One recorded outbound move.
pub struct AccountMigration {
    pub id: i64,
    pub target_acct: String,
    pub target_account_id: Option<i64>,
    pub followers_count: i64,
    pub created_at: OffsetDateTime,
}

/// Records a completed outbound move.
pub async fn record<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    target_acct: &str,
    target_account_id: i64,
    followers_count: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO account_migrations
            (id, account_id, target_acct, target_account_id, followers_count)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id::next(),
        account_id,
        target_acct,
        target_account_id,
        followers_count,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// When this account last moved, if ever — the cooldown input.
pub async fn latest_at(pool: &PgPool, account_id: i64) -> Result<Option<OffsetDateTime>, DbError> {
    let at = sqlx::query_scalar!(
        r#"
        SELECT max(created_at) AS "at"
        FROM account_migrations
        WHERE account_id = $1
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(at)
}

/// This account's move history, newest first.
pub async fn list(pool: &PgPool, account_id: i64) -> Result<Vec<AccountMigration>, DbError> {
    let rows = sqlx::query_as!(
        AccountMigration,
        r#"
        SELECT id AS "id!", target_acct AS "target_acct!", target_account_id,
               followers_count AS "followers_count!", created_at AS "created_at!"
        FROM account_migrations
        WHERE account_id = $1
        ORDER BY created_at DESC, id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
