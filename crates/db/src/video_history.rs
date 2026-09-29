//! Bounded, opt-in private viewing history; preference changes fence writers.
use crate::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

pub const MAX_ENTRIES: i64 = 1000;
pub const RETENTION_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct Preferences {
    pub history_enabled: bool,
    pub p2p_enabled: bool,
    pub p2p_configured: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            history_enabled: false,
            p2p_enabled: true,
            p2p_configured: false,
        }
    }
}

pub async fn preferences(pool: &PgPool, account: i64) -> Result<Preferences, DbError> {
    Ok(sqlx::query_as(
        "SELECT history_enabled,p2p_enabled,p2p_configured FROM video_viewer_preferences WHERE account_id=$1",
    )
    .bind(account)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default())
}

/// Partial settings preserve unrelated fields. Preference and optional history
/// deletion commit together; player opt-out cannot accidentally clear history.
pub async fn update_preferences(
    pool: &PgPool,
    account: i64,
    history: Option<bool>,
    p2p: Option<bool>,
) -> Result<Preferences, DbError> {
    let mut tx = pool.begin().await?;
    let prefs=sqlx::query_as("INSERT INTO video_viewer_preferences(account_id,history_enabled,p2p_enabled,p2p_configured) VALUES ($1,COALESCE($2,false),COALESCE($3,true),$3 IS NOT NULL) ON CONFLICT (account_id) DO UPDATE SET history_enabled=COALESCE($2,video_viewer_preferences.history_enabled),p2p_enabled=COALESCE($3,video_viewer_preferences.p2p_enabled),p2p_configured=video_viewer_preferences.p2p_configured OR $3 IS NOT NULL RETURNING history_enabled,p2p_enabled,p2p_configured")
        .bind(account).bind(history).bind(p2p).fetch_one(&mut *tx).await?;
    if history == Some(false) {
        sqlx::query("DELETE FROM video_watch_history WHERE account_id=$1")
            .bind(account)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(prefs)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Entry {
    pub media_id: i64,
    pub position_seconds: i32,
    pub completed: bool,
    pub updated_at: OffsetDateTime,
}

pub async fn enabled(pool: &PgPool, account: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT history_enabled FROM video_viewer_preferences WHERE account_id=$1",
    )
    .bind(account)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false))
}

/// Turning history off also deletes it atomically. A queued writer cannot
/// re-create history after opt-out: it locks this same preference row.
pub async fn set_enabled(pool: &PgPool, account: i64, enabled: bool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO video_viewer_preferences (account_id,history_enabled) VALUES ($1,$2) ON CONFLICT (account_id) DO UPDATE SET history_enabled=EXCLUDED.history_enabled")
        .bind(account).bind(enabled).execute(&mut *tx).await?;
    if !enabled {
        sqlx::query("DELETE FROM video_watch_history WHERE account_id=$1")
            .bind(account)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Returns false when disabled. Callers validate the published, readable VOD.
pub async fn save(
    pool: &PgPool,
    account: i64,
    media: i64,
    seconds: i32,
    completed: bool,
) -> Result<bool, DbError> {
    save_revision(pool, account, media, seconds, completed, 0).await
}

/// An old player cannot save a position against a replacement film. The media
/// fence also rechecks duration after any concurrent content adoption.
pub async fn save_revision(
    pool: &PgPool,
    account: i64,
    media: i64,
    seconds: i32,
    completed: bool,
    revision: i64,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let on: Option<bool> = sqlx::query_scalar(
        "SELECT history_enabled FROM video_viewer_preferences WHERE account_id=$1 FOR UPDATE",
    )
    .bind(account)
    .fetch_optional(&mut *tx)
    .await?;
    if on != Some(true) {
        return Ok(false);
    }
    let current:Option<(i64,Option<f64>)>=sqlx::query_as("SELECT video_content_revision,duration FROM media_attachments WHERE id=$1 AND status_id IS NOT NULL AND live_state IS NULL FOR SHARE")
        .bind(media).fetch_optional(&mut *tx).await?;
    let Some((current, duration)) = current else {
        return Ok(false);
    };
    if current != revision
        || !(0..=604_800).contains(&seconds)
        || duration.is_some_and(|d| {
            d.is_finite()
                && (f64::from(seconds) > d.ceil()
                    || completed && f64::from(seconds) < (d - 10.0).max(0.0))
        })
    {
        return Ok(false);
    }
    sqlx::query("INSERT INTO video_watch_history (account_id,media_id,position_seconds,completed,content_revision) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (account_id,media_id) DO UPDATE SET position_seconds=EXCLUDED.position_seconds,completed=EXCLUDED.completed,content_revision=EXCLUDED.content_revision,updated_at=now()")
        .bind(account).bind(media).bind(seconds).bind(completed).bind(revision).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM video_watch_history WHERE account_id=$1 AND (updated_at < now()-interval '90 days' OR media_id IN (SELECT media_id FROM video_watch_history WHERE account_id=$1 ORDER BY updated_at DESC,media_id DESC OFFSET 1000))")
        .bind(account).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn recent(pool: &PgPool, account: i64) -> Result<Vec<Entry>, DbError> {
    Ok(sqlx::query_as("SELECT h.media_id,h.position_seconds,h.completed,h.updated_at FROM video_watch_history h JOIN video_viewer_preferences p ON p.account_id=h.account_id AND p.history_enabled JOIN media_attachments m ON m.id=h.media_id AND m.video_content_revision=h.content_revision WHERE h.account_id=$1 AND h.updated_at >= now()-interval '90 days' ORDER BY h.updated_at DESC,h.media_id DESC LIMIT 1000")
        .bind(account).fetch_all(pool).await?)
}

pub async fn page(
    pool: &PgPool,
    account: i64,
    before: Option<OffsetDateTime>,
    before_media: i64,
) -> Result<Vec<Entry>, DbError> {
    Ok(sqlx::query_as("SELECT h.media_id,h.position_seconds,h.completed,h.updated_at FROM video_watch_history h JOIN video_viewer_preferences p ON p.account_id=h.account_id AND p.history_enabled JOIN media_attachments m ON m.id=h.media_id AND m.video_content_revision=h.content_revision WHERE h.account_id=$1 AND h.updated_at >= now()-interval '90 days' AND ($2::timestamptz IS NULL OR (h.updated_at,h.media_id)<($2,$3)) ORDER BY h.updated_at DESC,h.media_id DESC LIMIT 21")
        .bind(account).bind(before).bind(before_media).fetch_all(pool).await?)
}

pub async fn for_media(pool: &PgPool, account: i64, media: &[i64]) -> Result<Vec<Entry>, DbError> {
    Ok(sqlx::query_as("SELECT h.media_id,h.position_seconds,h.completed,h.updated_at FROM video_watch_history h JOIN video_viewer_preferences p ON p.account_id=h.account_id AND p.history_enabled JOIN media_attachments m ON m.id=h.media_id AND m.video_content_revision=h.content_revision WHERE h.account_id=$1 AND h.media_id=ANY($2) AND h.updated_at >= now()-interval '90 days'")
        .bind(account).bind(media).fetch_all(pool).await?)
}

/// Locks the preference before deletion, ordering concurrent saves before it.
pub async fn forget(pool: &PgPool, account: i64, media: Option<i64>) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT account_id FROM video_viewer_preferences WHERE account_id=$1 FOR UPDATE")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM video_watch_history WHERE account_id=$1 AND ($2::bigint IS NULL OR media_id=$2)")
        .bind(account).bind(media).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Hourly native maintenance, bounded per batch even for idle accounts.
pub async fn expire(pool: &PgPool) -> Result<u64, DbError> {
    Ok(sqlx::query("DELETE FROM video_watch_history WHERE (account_id,media_id) IN (SELECT account_id,media_id FROM video_watch_history WHERE updated_at < now()-interval '90 days' ORDER BY updated_at LIMIT 1000 FOR UPDATE SKIP LOCKED)")
        .execute(pool).await?.rows_affected())
}
