//! Private, bounded recurring imports. A source never owns publication rights.
use crate::{DbError, PgPool, id, video_draft};
use sqlx::{PgConnection, Postgres, Transaction};
use time::OffsetDateTime;

pub const MAX_SOURCES: i64 = 5;
pub const MAX_HISTORY: i64 = 10_000;

#[derive(Clone, sqlx::FromRow)]
pub struct Source {
    pub id: i64,
    pub account_id: i64,
    pub channel_id: i64,
    pub source_url: String,
    pub interval_minutes: i32,
    pub enabled: bool,
    pub generation: i64,
    pub lease_until: Option<OffsetDateTime>,
    pub cursor_url: Option<String>,
    pub cursor_position: i32,
    pub last_started_at: Option<OffsetDateTime>,
    pub last_checked_at: Option<OffsetDateTime>,
    pub next_check_at: OffsetDateTime,
    pub last_result: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Enqueued {
    Copied(i64),
    Seen,
    Quota,
    Lost,
    HistoryLimit,
}

pub async fn create(
    pool: &PgPool,
    owner: i64,
    channel: i64,
    url: &str,
    interval: i32,
) -> Result<Option<i64>, DbError> {
    let mut tx = pool.begin().await?;
    // Serialize the per-account limit across different destination channels.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,94011))")
        .bind(owner.to_string())
        .execute(&mut *tx)
        .await?;
    if !(15..=10080).contains(&interval)
        || url.is_empty()
        || url.len() > 2048
        || !crate::video::lock_owned_channel(&mut tx, channel, owner).await?
    {
        return Ok(None);
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM video_source_syncs WHERE account_id=$1")
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_SOURCES {
        return Ok(None);
    }
    let sync = id::next();
    let saved:Option<i64>=sqlx::query_scalar("INSERT INTO video_source_syncs(id,account_id,channel_id,source_url,interval_minutes) VALUES($1,$2,$3,$4,$5) ON CONFLICT(account_id,channel_id,source_url) DO NOTHING RETURNING id")
        .bind(sync).bind(owner).bind(channel).bind(url).bind(interval).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(saved)
}

pub async fn list(pool: &PgPool, owner: i64) -> Result<Vec<Source>, DbError> {
    Ok(sqlx::query_as(
        "SELECT * FROM video_source_syncs WHERE account_id=$1 ORDER BY id DESC LIMIT 5",
    )
    .bind(owner)
    .fetch_all(pool)
    .await?)
}

/// Pause discovery only; existing private drafts stay available for review.
/// Renewals fence an in-flight feed fetch before any further allocation.
pub async fn set_enabled(
    pool: &PgPool,
    sync: i64,
    owner: i64,
    enabled: bool,
) -> Result<bool, DbError> {
    Ok(sqlx::query("UPDATE video_source_syncs SET enabled=$3,generation=generation+1,lease_until=NULL,
        last_result=CASE WHEN $3 THEN 'pending' ELSE last_result END,
        next_check_at=CASE WHEN $3 THEN GREATEST(now(),COALESCE(last_started_at,now()-interval '1 minute')+interval '1 minute') ELSE next_check_at END
        WHERE id=$1 AND account_id=$2 AND enabled<>$3").bind(sync).bind(owner).bind(enabled).execute(pool).await?.rows_affected()==1)
}

pub async fn check_now(pool: &PgPool, sync: i64, owner: i64) -> Result<bool, DbError> {
    Ok(sqlx::query("UPDATE video_source_syncs SET next_check_at=now(),last_result='pending' WHERE id=$1 AND account_id=$2 AND enabled
        AND (lease_until IS NULL OR lease_until<=now()) AND (last_started_at IS NULL OR last_started_at<=now()-interval '1 minute')")
        .bind(sync).bind(owner).execute(pool).await?.rows_affected()==1)
}

pub async fn remove(pool: &PgPool, sync: i64, owner: i64) -> Result<bool, DbError> {
    Ok(
        sqlx::query("DELETE FROM video_source_syncs WHERE id=$1 AND account_id=$2")
            .bind(sync)
            .bind(owner)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

const AUTHORITY: &str = "SELECT EXISTS(SELECT 1 FROM video_channels c JOIN accounts ca ON ca.id=c.account_id
    JOIN accounts o ON o.id=c.owner_account_id JOIN accounts a ON a.id=$2 WHERE c.account_id=$1
    AND ca.suspended_at IS NULL AND o.suspended_at IS NULL AND a.suspended_at IS NULL
    AND (c.owner_account_id=$2 OR EXISTS(SELECT 1 FROM video_channel_grants g WHERE g.channel_id=$1 AND g.account_id=$2 AND g.accepted)))";

pub async fn claim(pool: &PgPool) -> Result<Option<Source>, DbError> {
    let mut tx = pool.begin().await?;
    let candidates:Vec<Source>=sqlx::query_as("SELECT * FROM video_source_syncs WHERE enabled AND next_check_at<=clock_timestamp()
        AND (lease_until IS NULL OR lease_until<=clock_timestamp()) ORDER BY next_check_at,id FOR UPDATE SKIP LOCKED LIMIT 20").fetch_all(&mut *tx).await?;
    for source in candidates {
        let allowed: bool = sqlx::query_scalar(AUTHORITY)
            .bind(source.channel_id)
            .bind(source.account_id)
            .fetch_one(&mut *tx)
            .await?;
        if !allowed {
            sqlx::query("UPDATE video_source_syncs SET enabled=false,generation=generation+1,lease_until=NULL WHERE id=$1").bind(source.id).execute(&mut *tx).await?;
            continue;
        }
        let claimed=sqlx::query_as("UPDATE video_source_syncs SET generation=generation+1,lease_until=clock_timestamp()+interval '120 seconds',last_started_at=clock_timestamp() WHERE id=$1 RETURNING *")
            .bind(source.id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        return Ok(Some(claimed));
    }
    tx.commit().await?;
    Ok(None)
}

pub async fn renew(pool: &PgPool, source: &Source) -> Result<bool, DbError> {
    let allowed: bool = sqlx::query_scalar(AUTHORITY)
        .bind(source.channel_id)
        .bind(source.account_id)
        .fetch_one(pool)
        .await?;
    if !allowed {
        return Ok(false);
    }
    Ok(sqlx::query(
        "UPDATE video_source_syncs SET lease_until=clock_timestamp()+interval '120 seconds'
        WHERE id=$1 AND generation=$2 AND enabled AND lease_until>clock_timestamp()",
    )
    .bind(source.id)
    .bind(source.generation)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub async fn seen(conn: &mut PgConnection, sync: i64, uri: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM video_source_sync_items WHERE sync_id=$1 AND source_uri=$2)",
    )
    .bind(sync)
    .bind(uri)
    .fetch_one(conn)
    .await?)
}

async fn lock_claim(tx: &mut Transaction<'_, Postgres>, source: &Source) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar::<_,i64>("SELECT id FROM video_source_syncs WHERE id=$1 AND generation=$2 AND enabled AND lease_until>clock_timestamp() FOR UPDATE")
        .bind(source.id).bind(source.generation).fetch_optional(&mut **tx).await?.is_some())
}

/// Allocation, quota, deduplication and durable download registration commit
/// together. Losing permission or a claim cannot leave an orphan reservation.
pub async fn enqueue(
    pool: &PgPool,
    source: &Source,
    uri: &str,
    new: &video_draft::NewDraft<'_>,
    limits: &video_draft::AdmissionLimits,
    metadata: &crate::video_metadata::Metadata,
) -> Result<Enqueued, DbError> {
    if metadata.media_id != new.media_id
        || new.account_id != source.account_id
        || new.channel_id != source.channel_id
        || uri.len() > 2048
        || uri.is_empty()
    {
        return Ok(Enqueued::Lost);
    }
    let mut tx = pool.begin().await?;
    // Match ordinary admission's resource -> channel -> work-row lock order.
    crate::video_storage::lock(&mut tx).await?;
    if !crate::video::lock_owned_channel(&mut tx, source.channel_id, source.account_id).await?
        || !lock_claim(&mut tx, source).await?
    {
        return Ok(Enqueued::Lost);
    }
    if seen(&mut tx, source.id, uri).await? {
        return Ok(Enqueued::Seen);
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM video_source_sync_items WHERE sync_id=$1")
            .bind(source.id)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_HISTORY {
        return Ok(Enqueued::HistoryLimit);
    }
    match video_draft::reserve_in_transaction(&mut tx, new, limits).await? {
        video_draft::Admission::Accepted => {}
        video_draft::Admission::Forbidden => return Ok(Enqueued::Lost),
        _ => return Ok(Enqueued::Quota),
    }
    crate::video_metadata::upsert(&mut *tx, metadata).await?;
    sqlx::query("INSERT INTO video_source_imports(media_id,source_url,source_kind) VALUES($1,$2,'activitypub')").bind(new.media_id).bind(uri).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO video_source_sync_items(sync_id,source_uri,media_id) VALUES($1,$2,$3)",
    )
    .bind(source.id)
    .bind(uri)
    .bind(new.media_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Enqueued::Copied(new.media_id))
}

pub async fn finish(
    pool: &PgPool,
    source: &Source,
    result: &str,
    cursor: Option<&str>,
) -> Result<bool, DbError> {
    finish_at(pool, source, result, cursor, 0).await
}

pub async fn finish_at(
    pool: &PgPool,
    source: &Source,
    result: &str,
    cursor: Option<&str>,
    position: i32,
) -> Result<bool, DbError> {
    Ok(sqlx::query("UPDATE video_source_syncs SET lease_until=NULL,last_checked_at=clock_timestamp(),last_result=$3,cursor_url=$4,cursor_position=$5,
        next_check_at=clock_timestamp()+make_interval(mins=>interval_minutes),
        enabled=CASE WHEN $3='history_limit' THEN false ELSE enabled END
        WHERE id=$1 AND generation=$2 AND enabled AND lease_until>clock_timestamp()")
        .bind(source.id).bind(source.generation).bind(result).bind(cursor).bind(position).execute(pool).await?.rows_affected()==1)
}
