//! Native video ownership. Mutations share the caller's outbox transaction.

use sqlx::{PgConnection, PgExecutor};

use crate::DbError;

#[derive(Debug, Clone)]
pub struct Channel {
    pub account_id: i64,
    pub owner_account_id: i64,
}

#[derive(Debug, Clone)]
pub struct Publication {
    pub status_id: i64,
    pub channel_id: i64,
    pub media_id: i64,
    pub uuid: String,
    pub channel_uri: String,
    pub file_size: i64,
    pub poster_file_name: Option<String>,
    pub poster_width: Option<i32>,
    pub poster_height: Option<i32>,
    pub poster_alt: Option<String>,
    pub live_dvr_window_seconds: Option<i32>,
    pub live_latency_mode: Option<i32>,
    pub live_scheduled_at: Option<time::OffsetDateTime>,
}

pub async fn create_channel(
    conn: &mut PgConnection,
    account_id: i64,
    owner_account_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET actor_type = 'Group' WHERE id = $1",
        account_id
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO video_channels (account_id, owner_account_id) VALUES ($1, $2)",
        account_id,
        owner_account_id,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn channel<'e, E: PgExecutor<'e>>(
    conn: E,
    account_id: i64,
) -> Result<Option<Channel>, DbError> {
    Ok(sqlx::query_as!(
        Channel,
        "SELECT account_id, owner_account_id FROM video_channels WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(conn)
    .await?)
}

pub async fn owned_channels<'e, E: PgExecutor<'e>>(
    conn: E,
    owner: i64,
) -> Result<Vec<Channel>, DbError> {
    Ok(sqlx::query_as!(Channel,
        "SELECT account_id, owner_account_id FROM video_channels WHERE owner_account_id = $1 ORDER BY account_id",
        owner,
    ).fetch_all(conn).await?)
}

/// Lock ownership while publishing so a later ownership-transfer API can use
/// the same row lock and cannot race a publication's permission check.
pub async fn lock_owned_channel(
    conn: &mut PgConnection,
    channel_id: i64,
    owner: i64,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT c.account_id FROM video_channels c JOIN accounts a ON a.id = c.account_id JOIN accounts o ON o.id = c.owner_account_id WHERE c.account_id = $1 AND c.owner_account_id = $2 AND a.suspended_at IS NULL AND o.suspended_at IS NULL FOR SHARE OF c, a, o",
        channel_id, owner,
    ).fetch_optional(conn).await?.is_some())
}

pub async fn publish(
    conn: &mut PgConnection,
    status_id: i64,
    channel_id: i64,
    media_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        // Explicit film authoring overrides the ordinary attachment endpoint's
        // short-silent-clip heuristic. Client API films must not autoplay/loop.
        "WITH film AS (
             UPDATE media_attachments SET kind = 'video'
             WHERE id = $3 AND status_id = $1 RETURNING id
         )
         INSERT INTO video_publications (status_id, channel_id, media_id)
         SELECT $1, $2, id FROM film",
        status_id,
        channel_id,
        media_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Hold the media row through publication, fencing draft cancellation/expiry.
/// A durable draft is publishable only in its selected channel while ready and
/// unexpired. Ordinary completed client-API attachments remain supported.
pub async fn lock_publishable_media(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
    channel_id: i64,
    replay_session_id: Option<&str>,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT m.id FROM media_attachments m LEFT JOIN video_drafts d ON d.media_id = m.id
         WHERE m.id = $1 AND m.account_id = $2 AND m.status_id IS NULL AND m.processing = 'complete'
           AND (d.media_id IS NULL OR (d.account_id = $2 AND d.channel_id = $3
                AND d.state = 'ready' AND d.expires_at > clock_timestamp()))
           AND (NOT EXISTS (SELECT 1 FROM native_live_sessions ls WHERE ls.replay_media_id = m.id)
                OR EXISTS (SELECT 1 FROM native_live_sessions ls
                    JOIN native_live_broadcasts b ON b.media_id = ls.media_id
                    WHERE ls.replay_media_id = m.id AND ls.session_id = $4
                      AND ls.ended_at IS NOT NULL AND b.permanent
                      AND b.owner_account_id = $2 AND b.channel_id = $3))
         FOR UPDATE OF m",
        media_id,
        owner,
        channel_id,
        replay_session_id,
    )
    .fetch_optional(conn)
    .await?
    .is_some())
}

/// A channel's local timeline entry represents the same Announce sent to peers.
/// Copy the source audience: promoting an unlisted film to a public boost would
/// leak it into discovery. This is not a community submission or moderator vote.
pub async fn create_announcement(
    conn: &mut PgConnection,
    channel_id: i64,
    status_id: i64,
    uri: &str,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"INSERT INTO statuses (id, uri, account_id, content, visibility, reblog_of_id,
                                sensitive, language, created_at, sort_at)
           SELECT $1, $2, $3, '', s.visibility, s.id, s.sensitive, s.language,
                  s.created_at, s.sort_at
           FROM statuses s WHERE s.id = $4 AND s.visibility IN ('public', 'unlisted')
           ON CONFLICT (account_id, reblog_of_id) WHERE reblog_of_id IS NOT NULL
           DO UPDATE SET uri = EXCLUDED.uri
           RETURNING id"#,
        crate::id::next(),
        uri,
        channel_id,
        status_id,
    )
    .fetch_one(conn)
    .await?)
}

pub async fn publications<'e, E: PgExecutor<'e>>(
    conn: E,
    ids: &[i64],
) -> Result<Vec<Publication>, DbError> {
    Ok(sqlx::query_as!(
        Publication,
        r#"SELECT v.status_id, v.channel_id, v.media_id, v.uuid,
                  a.uri AS "channel_uri!", COALESCE(m.file_size, 0) AS "file_size!",
                  p.file_name AS "poster_file_name?", p.width AS "poster_width?", p.height AS "poster_height?",
                  d.poster_alt AS "poster_alt?",
                  b.dvr_window_seconds AS "live_dvr_window_seconds?",
                  b.latency_mode AS "live_latency_mode?",
                  b.scheduled_at AS "live_scheduled_at?"
           FROM video_publications v JOIN accounts a ON a.id = v.channel_id
           JOIN media_attachments m ON m.id = v.media_id
           LEFT JOIN video_compatibility_posters p ON p.media_id = v.media_id
           LEFT JOIN video_drafts d ON d.media_id = v.media_id
           LEFT JOIN native_live_broadcasts b ON b.media_id = v.media_id
           WHERE v.status_id = ANY($1)"#,
        ids,
    )
    .fetch_all(conn)
    .await?)
}

#[derive(Debug)]
pub struct OutboxEntry {
    pub status_id: i64,
    pub uuid: String,
    pub status_uri: Option<String>,
    pub author_uri: Option<String>,
    pub username: String,
    pub created_at: time::OffsetDateTime,
    pub visibility: String,
}

/// Public channel history; moderation and both owner/channel relationships
/// apply before pagination. Future restricted publications must not leak here.
pub async fn outbox_page(
    pool: &sqlx::PgPool,
    channel_id: i64,
    viewer: Option<i64>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<OutboxEntry>, DbError> {
    Ok(sqlx::query_as!(
        OutboxEntry,
        r#"SELECT v.status_id, v.uuid, s.uri AS status_uri, a.uri AS author_uri,
                  a.username, s.created_at, s.visibility
           FROM video_publications v JOIN statuses s ON s.id = v.status_id
           JOIN accounts a ON a.id = s.account_id
           WHERE v.channel_id = $1 AND s.visibility IN ('public', 'unlisted')
             AND s.deleted_at IS NULL -- STUBFILTER
             AND NOT account_hidden($2, s.account_id) AND NOT account_hidden($2, v.channel_id)
             AND ($3::bigint IS NULL OR s.id < $3)
           ORDER BY s.id DESC LIMIT $4"#,
        channel_id,
        viewer,
        before,
        limit,
    )
    .fetch_all(pool)
    .await?)
}

pub async fn outbox_count(
    pool: &sqlx::PgPool,
    channel_id: i64,
    viewer: Option<i64>,
) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM video_publications v JOIN statuses s ON s.id = v.status_id
           WHERE v.channel_id = $1 AND s.visibility IN ('public', 'unlisted')
             AND s.deleted_at IS NULL -- STUBFILTER
             AND NOT account_hidden($2, s.account_id) AND NOT account_hidden($2, v.channel_id)"#,
        channel_id, viewer,
    ).fetch_one(pool).await?)
}
