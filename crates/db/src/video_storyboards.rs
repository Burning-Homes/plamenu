//! Immutable native seek-preview metadata, batched like captions/chapters.
use crate::DbError;
use sqlx::{FromRow, PgConnection, PgExecutor};
use std::collections::HashMap;

#[derive(Debug, Clone, FromRow)]
pub struct Storyboard {
    pub media_id: i64,
    pub generation: i64,
    pub file_name: String,
    pub size_bytes: i64,
    pub columns: i32,
    pub rows: i32,
    pub tile_count: i32,
    pub interval_seconds: i32,
}

pub async fn for_media<'e, E: PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> Result<HashMap<i64, Storyboard>, DbError> {
    let rows = sqlx::query_as::<_, Storyboard>("SELECT b.* FROM video_storyboards b
        JOIN video_drafts d ON d.media_id=b.media_id AND d.generation=b.generation AND d.state='ready'
        WHERE b.media_id=ANY($1)").bind(ids).fetch_all(executor).await?;
    Ok(rows.into_iter().map(|row| (row.media_id, row)).collect())
}

/// Caller holds the current claim fence; the sprite joins readiness atomically.
pub async fn replace(conn: &mut PgConnection, row: &Storyboard) -> Result<(), DbError> {
    sqlx::query("INSERT INTO video_storyboards
        (media_id,generation,file_name,size_bytes,columns,rows,tile_count,interval_seconds)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (media_id) DO UPDATE SET
        generation=EXCLUDED.generation,file_name=EXCLUDED.file_name,size_bytes=EXCLUDED.size_bytes,
        columns=EXCLUDED.columns,rows=EXCLUDED.rows,tile_count=EXCLUDED.tile_count,interval_seconds=EXCLUDED.interval_seconds")
        .bind(row.media_id).bind(row.generation).bind(&row.file_name).bind(row.size_bytes)
        .bind(row.columns).bind(row.rows).bind(row.tile_count).bind(row.interval_seconds)
        .execute(conn).await?;
    Ok(())
}
