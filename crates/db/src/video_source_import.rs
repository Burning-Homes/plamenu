//! Fenced source downloads. The receiving draft owns quota and source cleanup.
use crate::{DbError, PgPool};
use sqlx::PgConnection;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claim {
    pub media_id: i64,
    pub account_id: i64,
    pub channel_id: i64,
    pub source_url: String,
    pub source_kind: String,
    pub generation: i64,
    pub reserved_bytes: i64,
    pub target_media_id: Option<i64>,
    pub target_revision: Option<i64>,
}

/// Lock the receiving draft before registering a job, just like the finalizer.
pub async fn enqueue(
    pool: &PgPool,
    id: i64,
    owner: i64,
    url: &str,
    kind: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    if !crate::video_draft::lock_receiving(&mut tx, id, owner).await? {
        return Ok(false);
    }
    sqlx::query!(
        "INSERT INTO video_source_imports (media_id, source_url, source_kind) VALUES ($1, $2, $3)",
        id,
        url,
        kind
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn phase(pool: &PgPool, id: i64) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT state FROM video_source_imports WHERE media_id = $1",
        id
    )
    .fetch_optional(pool)
    .await?)
}

/// One download slot in the native server; oldest submitted work first. Expired
/// attempts restart into distinct temporary files, never append uncertain bytes.
pub async fn claim(pool: &PgPool) -> Result<Option<Claim>, DbError> {
    Ok(sqlx::query_as::<_,Claim>("UPDATE video_source_imports i SET state = 'downloading',
        generation = i.generation + 1, lease_until = clock_timestamp() + interval '120 seconds'
        FROM video_drafts d LEFT JOIN video_source_edits e ON e.media_id=d.media_id
        WHERE i.media_id = d.media_id AND i.media_id = (
            SELECT q.media_id FROM video_source_imports q JOIN video_drafts v ON v.media_id = q.media_id
            JOIN media_attachments m ON m.id = v.media_id
            JOIN accounts a ON a.id = v.account_id JOIN accounts c ON c.id = v.channel_id
            WHERE (q.state = 'queued' OR (q.state = 'downloading' AND q.lease_until <= clock_timestamp()))
              AND v.state = 'receiving' AND v.expires_at > clock_timestamp() AND m.status_id IS NULL
              AND a.suspended_at IS NULL AND c.suspended_at IS NULL
              AND EXISTS (SELECT 1 FROM video_channels ch JOIN accounts own ON own.id = ch.owner_account_id
                WHERE ch.account_id = v.channel_id AND own.suspended_at IS NULL
                AND (ch.owner_account_id = v.account_id OR EXISTS (SELECT 1 FROM video_channel_grants g
                    WHERE g.channel_id = v.channel_id AND g.account_id = v.account_id AND g.accepted)))
            ORDER BY q.media_id FOR UPDATE OF q SKIP LOCKED LIMIT 1)
        RETURNING i.media_id, d.account_id, d.channel_id, i.source_url, i.source_kind, i.generation, d.reserved_bytes,
        e.target_media_id,e.target_revision")
        .fetch_optional(pool).await?)
}

pub async fn renew(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    Ok(sqlx::query!("UPDATE video_source_imports i SET lease_until = clock_timestamp() + interval '120 seconds'
        FROM video_drafts d, accounts a, accounts c WHERE i.media_id = $1 AND i.generation = $2
        AND i.state = 'downloading' AND i.lease_until > clock_timestamp()
        AND d.media_id = i.media_id AND d.state = 'receiving' AND d.expires_at > clock_timestamp()
        AND a.id = d.account_id AND c.id = d.channel_id AND a.suspended_at IS NULL AND c.suspended_at IS NULL
        AND EXISTS (SELECT 1 FROM video_channels ch JOIN accounts own ON own.id = ch.owner_account_id
            WHERE ch.account_id = d.channel_id AND own.suspended_at IS NULL
            AND (ch.owner_account_id = d.account_id OR EXISTS (SELECT 1 FROM video_channel_grants g
                WHERE g.channel_id = d.channel_id AND g.account_id = d.account_id AND g.accepted)))",
        claim.media_id, claim.generation).execute(pool).await?.rows_affected() == 1)
}

pub async fn lock_claim(conn: &mut PgConnection, claim: &Claim) -> Result<bool, DbError> {
    if !crate::video::lock_owned_channel(conn, claim.channel_id, claim.account_id).await? {
        return Ok(false);
    }
    if !crate::video_draft::lock_receiving(conn, claim.media_id, claim.account_id).await? {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        "SELECT media_id FROM video_source_imports WHERE media_id = $1 AND generation = $2
        AND state = 'downloading' AND lease_until > clock_timestamp() FOR UPDATE",
        claim.media_id,
        claim.generation
    )
    .fetch_optional(conn)
    .await?
    .is_some())
}

pub async fn complete(conn: &mut PgConnection, claim: &Claim) -> Result<(), DbError> {
    sqlx::query!("UPDATE video_source_imports SET state = 'done', lease_until = NULL, source_url = 'imported'
        WHERE media_id = $1 AND generation = $2", claim.media_id, claim.generation).execute(conn).await?;
    Ok(())
}

pub async fn fail(pool: &PgPool, claim: &Claim) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_source_imports SET state = 'failed', lease_until = NULL
        WHERE media_id = $1 AND generation = $2 AND state = 'downloading'",
        claim.media_id,
        claim.generation
    )
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn retry(pool: &PgPool, id: i64, owner: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let channel = sqlx::query_scalar!(
        "SELECT channel_id FROM video_drafts WHERE media_id = $1 AND account_id = $2",
        id,
        owner
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(channel) = channel else {
        return Ok(false);
    };
    if !crate::video::lock_owned_channel(&mut tx, channel, owner).await?
        || !crate::video_draft::lock_receiving(&mut tx, id, owner).await?
    {
        return Ok(false);
    }
    let changed = sqlx::query!(
        "UPDATE video_source_imports SET state = 'queued'
        WHERE media_id = $1 AND state = 'failed'",
        id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(changed)
}
