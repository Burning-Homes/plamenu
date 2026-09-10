//! Pictrs-shaped ownership metadata over Plamenu's native media rows.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Upload {
    pub media_id: i64,
    pub account_id: i64,
    pub alias: String,
    pub delete_token: String,
    pub created_at: OffsetDateTime,
}

pub async fn create(
    pool: &PgPool,
    media_id: i64,
    account_id: i64,
    alias: &str,
    delete_token: &str,
) -> Result<Upload, DbError> {
    sqlx::query_as::<_, Upload>(
        r"
        INSERT INTO lemmy_media_uploads (media_id, account_id, alias, delete_token)
        VALUES ($1, $2, $3, $4)
        RETURNING media_id, account_id, alias, delete_token, created_at
        ",
    )
    .bind(media_id)
    .bind(account_id)
    .bind(alias)
    .bind(delete_token)
    .fetch_one(pool)
    .await
    .map_err(DbError::from)
}

pub async fn find_capability(
    pool: &PgPool,
    alias: &str,
    delete_token: &str,
) -> Result<Option<Upload>, DbError> {
    sqlx::query_as::<_, Upload>(
        r"
        SELECT media_id, account_id, alias, delete_token, created_at
        FROM lemmy_media_uploads
        WHERE alias = $1 AND delete_token = $2
        ",
    )
    .bind(alias)
    .bind(delete_token)
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)
}

pub async fn list(
    pool: &PgPool,
    account_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Upload>, DbError> {
    sqlx::query_as::<_, Upload>(
        r"
        SELECT media_id, account_id, alias, delete_token, created_at
        FROM lemmy_media_uploads
        WHERE ($1::bigint IS NULL OR account_id = $1)
        ORDER BY created_at DESC, media_id DESC
        LIMIT $2 OFFSET $3
        ",
    )
    .bind(account_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}
