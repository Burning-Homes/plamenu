//! Channel authority, independent of a Video's immutable author identity.
//! Every authority mutation locks the channel exclusively; content mutations
//! retain a shared lock through commit, fencing revocation and owner transfer.
use crate::{DbError, video::Channel};
use sqlx::{FromRow, PgConnection, PgExecutor, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Grant {
    pub channel_id: i64,
    pub account_id: i64,
    pub role: String,
    pub accepted: bool,
}

pub async fn role<'e, E: PgExecutor<'e>>(
    conn: E,
    channel: i64,
    actor: i64,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT CASE WHEN c.owner_account_id=$2 THEN 'owner' ELSE g.role END
         FROM video_channels c JOIN accounts ca ON ca.id=c.account_id
         JOIN accounts o ON o.id=c.owner_account_id JOIN accounts a ON a.id=$2
         LEFT JOIN video_channel_grants g ON g.channel_id=c.account_id AND g.account_id=$2 AND g.accepted
         WHERE c.account_id=$1 AND ca.suspended_at IS NULL AND o.suspended_at IS NULL
           AND a.suspended_at IS NULL AND ca.deleted_at IS NULL AND o.deleted_at IS NULL
           AND a.deleted_at IS NULL AND (c.owner_account_id=$2 OR g.account_id IS NOT NULL)"
    ).bind(channel).bind(actor).fetch_optional(conn).await?)
}

pub async fn lock_access(
    conn: &mut PgConnection,
    channel: i64,
    actor: i64,
) -> Result<Option<String>, DbError> {
    // Take the channel lock BEFORE reading grants. In READ COMMITTED a revoked
    // grant cannot survive a wait behind the revoker's exclusive lock.
    let found: Option<i64> =
        sqlx::query_scalar("SELECT account_id FROM video_channels WHERE account_id=$1 FOR SHARE")
            .bind(channel)
            .fetch_optional(&mut *conn)
            .await?;
    if found.is_none() {
        return Ok(None);
    }
    // Account moderation uses row locks, so hold active identities too.
    sqlx::query("SELECT a.id FROM accounts a JOIN video_channels c ON c.account_id=$1 WHERE a.id IN (c.account_id,c.owner_account_id,$2) ORDER BY a.id FOR SHARE OF a")
        .bind(channel).bind(actor).fetch_all(&mut *conn).await?;
    role(conn, channel, actor).await
}

pub async fn available(pool: &PgPool, actor: i64) -> Result<Vec<Channel>, DbError> {
    Ok(sqlx::query_as::<_, Channel>(
        "SELECT c.account_id,c.owner_account_id FROM video_channels c
         JOIN accounts ca ON ca.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id
         WHERE ca.suspended_at IS NULL AND o.suspended_at IS NULL AND
           (c.owner_account_id=$1 OR EXISTS(SELECT 1 FROM video_channel_grants g
              WHERE g.channel_id=c.account_id AND g.account_id=$1 AND g.accepted))
         ORDER BY c.account_id",
    )
    .bind(actor)
    .fetch_all(pool)
    .await?)
}

pub async fn roles(
    pool: &PgPool,
    actor: i64,
) -> Result<std::collections::HashMap<i64, String>, DbError> {
    let rows:Vec<(i64,String)>=sqlx::query_as("SELECT c.account_id,CASE WHEN c.owner_account_id=$1 THEN 'owner' ELSE g.role END FROM video_channels c JOIN accounts ca ON ca.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id LEFT JOIN video_channel_grants g ON g.channel_id=c.account_id AND g.account_id=$1 AND g.accepted WHERE ca.suspended_at IS NULL AND o.suspended_at IS NULL AND (c.owner_account_id=$1 OR g.account_id IS NOT NULL)")
        .bind(actor).fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

pub async fn grants(pool: &PgPool, channel: i64) -> Result<Vec<Grant>, DbError> {
    Ok(sqlx::query_as("SELECT channel_id,account_id,role,accepted FROM video_channel_grants WHERE channel_id=$1 ORDER BY account_id")
        .bind(channel).fetch_all(pool).await?)
}
pub async fn invitations(pool: &PgPool, actor: i64) -> Result<Vec<i64>, DbError> {
    Ok(sqlx::query_scalar("SELECT c.account_id FROM video_channels c JOIN accounts ca ON ca.id=c.account_id JOIN accounts o ON o.id=c.owner_account_id WHERE ca.suspended_at IS NULL AND o.suspended_at IS NULL AND (c.transfer_to=$1 OR EXISTS(SELECT 1 FROM video_channel_grants g WHERE g.channel_id=c.account_id AND g.account_id=$1 AND NOT g.accepted)) ORDER BY c.account_id")
        .bind(actor).fetch_all(pool).await?)
}

/// Must be held through a content mutation. Publishers can manage only their
/// own channel posts; owners/editors can manage every publication in the channel.
pub async fn lock_publication(
    conn: &mut PgConnection,
    media: i64,
    actor: i64,
) -> Result<bool, DbError> {
    let row: Option<(i64,i64)> = sqlx::query_as("SELECT v.channel_id,s.account_id FROM video_publications v JOIN statuses s ON s.id=v.status_id WHERE v.media_id=$1 AND s.deleted_at IS NULL\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos")
        .bind(media).fetch_optional(&mut *conn).await?;
    let Some((channel, author)) = row else {
        return Ok(false);
    };
    Ok(match lock_access(conn, channel, actor).await?.as_deref() {
        Some("owner" | "editor") => true,
        Some("publisher") => author == actor,
        _ => false,
    })
}

pub async fn publication_author(
    pool: &PgPool,
    status: i64,
    actor: i64,
) -> Result<Option<i64>, DbError> {
    let row: Option<(i64,i64)> = sqlx::query_as("SELECT v.channel_id,s.account_id FROM video_publications v JOIN statuses s ON s.id=v.status_id WHERE v.status_id=$1 AND s.deleted_at IS NULL\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos")
        .bind(status).fetch_optional(pool).await?;
    let Some((channel, author)) = row else {
        return Ok(None);
    };
    Ok(match role(pool, channel, actor).await?.as_deref() {
        Some("owner" | "editor") => Some(author),
        Some("publisher") if author == actor => Some(author),
        _ => None,
    })
}
