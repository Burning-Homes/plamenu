//! Declared account aliases — Mastodon's `account_aliases`. One row per alias
//! a local account claims (the *destination* side of a move names the origin
//! here). The `accounts.also_known_as` array remains the wire-format source
//! for the actor document; callers keep the two in sync
//! (`crate::account::set_aliases`).

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// One declared alias: the acct as the user entered it (for display) and the
/// canonical actor URI (what `alsoKnownAs` carries).
pub struct AccountAlias {
    pub id: i64,
    pub acct: String,
    pub uri: String,
    pub created_at: OffsetDateTime,
}

/// Adds an alias row. Idempotent on `(account_id, uri)`; a re-add keeps the
/// original row (and its acct spelling).
pub async fn add(pool: &PgPool, account_id: i64, acct: &str, uri: &str) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO account_aliases (id, account_id, acct, uri)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, uri) DO NOTHING
        "#,
        id::next(),
        account_id,
        acct,
        uri,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Removes the alias with this URI. Returns whether a row was deleted.
pub async fn remove(pool: &PgPool, account_id: i64, uri: &str) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM account_aliases WHERE account_id = $1 AND uri = $2",
        account_id,
        uri,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// All aliases of an account, oldest first (declaration order).
pub async fn list(pool: &PgPool, account_id: i64) -> Result<Vec<AccountAlias>, DbError> {
    let rows = sqlx::query_as!(
        AccountAlias,
        r#"
        SELECT id AS "id!", acct AS "acct!", uri AS "uri!", created_at AS "created_at!"
        FROM account_aliases
        WHERE account_id = $1
        ORDER BY id
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
