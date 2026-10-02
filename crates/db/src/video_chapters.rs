//! Bounded, time-ordered chapters attached to a video.

use std::collections::HashMap;

use sqlx::{FromRow, PgConnection, PgExecutor, PgPool};

use crate::DbError;

pub const MAX_CHAPTERS: usize = 100;
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_START_SECONDS: i32 = 604_800;

#[derive(Debug, Clone, PartialEq, Eq, FromRow, serde::Deserialize)]
pub struct Chapter {
    pub media_id: i64,
    pub start_seconds: i32,
    pub title: String,
}

#[must_use]
pub fn valid_chapter(start_seconds: i32, title: &str) -> bool {
    (0..=MAX_START_SECONDS).contains(&start_seconds)
        && !title.trim().is_empty()
        && title.chars().count() <= MAX_TITLE_CHARS
}

pub async fn for_media<'e, E: PgExecutor<'e>>(
    executor: E,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<Chapter>>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, Chapter>(
        "SELECT media_id, start_seconds, title FROM video_chapters
         WHERE media_id = ANY($1) ORDER BY media_id, start_seconds",
    )
    .bind(media_ids)
    .fetch_all(executor)
    .await?;
    let mut out: HashMap<i64, Vec<Chapter>> = HashMap::new();
    for row in rows {
        out.entry(row.media_id).or_default().push(row);
    }
    Ok(out)
}

pub async fn replace_remote(
    pool: &PgPool,
    media_id: i64,
    chapters: &[Chapter],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    replace_conn(&mut tx, media_id, chapters).await?;
    tx.commit().await?;
    Ok(())
}

/// The caller locks and validates ownership before writing; this leaves the
/// chapter set in the same transaction as a signed Video Update when published.
pub async fn replace_conn(
    conn: &mut PgConnection,
    media_id: i64,
    chapters: &[Chapter],
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM video_chapters WHERE media_id = $1")
        .bind(media_id)
        .execute(&mut *conn)
        .await?;
    for chapter in chapters {
        sqlx::query(
            "INSERT INTO video_chapters (media_id, start_seconds, title)
             VALUES ($1, $2, $3)",
        )
        .bind(media_id)
        .bind(chapter.start_seconds)
        .bind(&chapter.title)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn replace_draft(
    pool: &PgPool,
    media_id: i64,
    owner: i64,
    chapters: &[Chapter],
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let authorized = sqlx::query_scalar::<_, i64>(
        "SELECT m.id FROM media_attachments m
         JOIN video_drafts d ON d.media_id = m.id
         JOIN accounts a ON a.id = d.account_id
         JOIN accounts ch ON ch.id = d.channel_id
         WHERE m.id = $1 AND m.account_id = $2 AND d.account_id = $2
           AND m.status_id IS NULL AND d.state = 'ready'
           AND d.expires_at > clock_timestamp()
           AND a.suspended_at IS NULL AND ch.suspended_at IS NULL
         FOR UPDATE OF m",
    )
    .bind(media_id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    if authorized.is_none() {
        return Ok(false);
    }
    replace_conn(&mut tx, media_id, chapters).await?;
    tx.commit().await?;
    Ok(true)
}
