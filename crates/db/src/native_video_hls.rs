//! Atomic local HLS metadata. The corresponding fMP4 bodies are immutable
//! attempt outputs; neither this table nor a playlist is a storage journal.

use sqlx::{PgConnection, PgPool};
use std::collections::HashMap;

use crate::DbError;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Rendition {
    pub media_id: i64,
    pub generation: i64,
    pub height: i32,
    pub width: i32,
    pub bandwidth: i64,
    pub size_bytes: i64,
    pub file_name: String,
    pub playlist: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LadderEntry {
    pub height: i32,
    pub width: i32,
    pub bandwidth: i64,
}

/// Small `ActivityPub` link metadata. Never load playlist bodies for a page of
/// federated objects; those can be megabytes for long videos.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct FileTag {
    pub media_id: i64,
    pub height: i32,
    pub width: i32,
    pub size_bytes: i64,
    pub file_name: String,
}

pub async fn tags_for_media(
    conn: &mut PgConnection,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<FileTag>>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        FileTag,
        "SELECT r.media_id, r.height, r.width, r.size_bytes, r.file_name
         FROM native_video_hls_renditions r
         JOIN video_drafts d ON d.media_id = r.media_id AND d.generation = r.generation AND d.state = 'ready'
         WHERE r.media_id = ANY($1)
         ORDER BY r.height DESC",
        media_ids,
    )
    .fetch_all(conn)
    .await?;
    let mut by_media: HashMap<i64, Vec<FileTag>> = HashMap::new();
    for row in rows {
        by_media.entry(row.media_id).or_default().push(row);
    }
    Ok(by_media)
}

pub async fn replace(
    conn: &mut PgConnection,
    media_id: i64,
    generation: i64,
    renditions: &[Rendition],
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM native_video_hls_renditions WHERE media_id = $1",
        media_id,
    )
    .execute(&mut *conn)
    .await?;
    for rendition in renditions {
        sqlx::query!(
            "INSERT INTO native_video_hls_renditions
             (media_id, generation, height, width, bandwidth, size_bytes, file_name, playlist)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            media_id,
            generation,
            rendition.height,
            rendition.width,
            rendition.bandwidth,
            rendition.size_bytes,
            rendition.file_name,
            rendition.playlist,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Only committed, unsuspended publications expose playlist metadata. The
/// file route has an independent check before serving the referenced bytes.
pub async fn public_ladder(pool: &PgPool, media_id: i64) -> Result<Vec<LadderEntry>, DbError> {
    Ok(sqlx::query_as!(
        LadderEntry,
        "SELECT r.height, r.width, r.bandwidth
         FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN video_publications v ON v.media_id = m.id AND v.status_id = m.status_id
         JOIN accounts a ON a.id = m.account_id AND a.suspended_at IS NULL
         JOIN accounts c ON c.id = v.channel_id AND c.suspended_at IS NULL
         WHERE r.media_id = $1
         ORDER BY r.height DESC",
        media_id,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn public_playlist(
    pool: &PgPool,
    media_id: i64,
    height: i32,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT r.playlist FROM native_video_hls_renditions r
         JOIN media_attachments m ON m.id = r.media_id AND m.processing = 'complete'
         JOIN video_drafts d ON d.media_id = m.id AND d.generation = r.generation AND d.state = 'ready'
         JOIN video_publications v ON v.media_id = m.id AND v.status_id = m.status_id
         JOIN accounts a ON a.id = m.account_id AND a.suspended_at IS NULL
         JOIN accounts c ON c.id = v.channel_id AND c.suspended_at IS NULL
         WHERE r.media_id = $1 AND r.height = $2",
        media_id,
        height,
    )
    .fetch_optional(pool)
    .await?)
}
