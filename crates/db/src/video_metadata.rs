//! Bounded Video authoring options and interoperable category/licence IDs.
use crate::DbError;
use sqlx::{PgConnection, PgExecutor};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Metadata {
    pub media_id: i64,
    pub category: Option<i32>,
    pub licence: Option<i32>,
    pub spoiler_text: String,
    pub sensitive: bool,
    pub licence_version_known: bool,
}

pub async fn for_media<'e, E: PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> Result<HashMap<i64, Metadata>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(sqlx::query_as!(Metadata, "SELECT media_id, category, licence, spoiler_text, sensitive, licence_version_known FROM video_metadata WHERE media_id = ANY($1)", ids)
        .fetch_all(executor).await?.into_iter().map(|row| (row.media_id, row)).collect())
}

pub async fn upsert<'e, E: PgExecutor<'e>>(executor: E, value: &Metadata) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO video_metadata (media_id, category, licence, spoiler_text, sensitive, licence_version_known)
        VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (media_id) DO UPDATE SET
        category = EXCLUDED.category, licence = EXCLUDED.licence,
        spoiler_text = EXCLUDED.spoiler_text, sensitive = EXCLUDED.sensitive,
        licence_version_known = EXCLUDED.licence_version_known",
        value.media_id,
        value.category,
        value.licence,
        value.spoiler_text,
        value.sensitive,
        value.licence_version_known
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Reject mutation of an attached or scheduled upload, under its media lock.
pub async fn lock_owned_draft(
    conn: &mut PgConnection,
    owner: i64,
    media_id: i64,
) -> Result<bool, DbError> {
    let row = sqlx::query_scalar!(
        "SELECT id FROM media_attachments WHERE id = $1 AND account_id = $2
        AND status_id IS NULL AND scheduled_status_id IS NULL AND remote_url IS NULL FOR UPDATE",
        media_id,
        owner
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}
