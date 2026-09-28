//! Bounded ordered Video collections. Runtime-typed reads share the same row
//! shape for the local and remote paths; server integration tests cover them.
use crate::DbError;
use sqlx::{FromRow, PgExecutor};
use time::OffsetDateTime;

pub const MAX_ENTRIES: usize = 1000;
pub const MAX_PLAYLISTS: i64 = 100;

#[derive(Debug, Clone, FromRow)]
pub struct Playlist {
    pub id: i64,
    pub uuid: String,
    pub creator_id: i64,
    pub channel_id: i64,
    pub remote_uri: Option<String>,
    pub name: String,
    pub description: String,
    pub visibility: String,
    pub revision: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Entry {
    pub id: i64,
    pub playlist_id: i64,
    pub remote_uri: Option<String>,
    pub status_id: Option<i64>,
    pub video_uri: String,
    pub position: i32,
    pub start_seconds: i32,
    pub stop_seconds: Option<i32>,
}

pub async fn find<'e, E: PgExecutor<'e>>(conn: E, id: i64) -> Result<Option<Playlist>, DbError> {
    Ok(sqlx::query_as("SELECT * FROM video_playlists WHERE id=$1")
        .bind(id)
        .fetch_optional(conn)
        .await?)
}
pub async fn by_uri<'e, E: PgExecutor<'e>>(
    conn: E,
    uri: &str,
) -> Result<Option<Playlist>, DbError> {
    Ok(
        sqlx::query_as("SELECT * FROM video_playlists WHERE remote_uri=$1")
            .bind(uri)
            .fetch_optional(conn)
            .await?,
    )
}
pub async fn entries<'e, E: PgExecutor<'e>>(conn: E, id: i64) -> Result<Vec<Entry>, DbError> {
    Ok(sqlx::query_as(
        "SELECT * FROM video_playlist_entries WHERE playlist_id=$1 ORDER BY position,id",
    )
    .bind(id)
    .fetch_all(conn)
    .await?)
}
#[must_use]
pub fn valid_clip(start: i32, stop: Option<i32>, duration: Option<f64>) -> bool {
    (0..=604_800).contains(&start)
        && stop.is_none_or(|end| end > start && end <= 604_800)
        && duration.is_none_or(|seconds| {
            f64::from(start) < seconds && stop.is_none_or(|end| f64::from(end) <= seconds)
        })
}
