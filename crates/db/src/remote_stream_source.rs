//! Positively detected federated live-stream sources.
//!
//! An Owncast account has two useful media identities: each federated go-live
//! Note gets a status attachment, while the account itself owns one unattached
//! live attachment so an already-running stream discovered by its homepage can
//! be watched without inventing a status that was never federated.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id, media};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RemoteStreamSource {
    pub account_id: i64,
    pub media_id: i64,
    pub provider: String,
    pub homepage_url: String,
    pub status_url: String,
    pub hls_master_url: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Stores or refreshes one `Owncast` capability and its account-level player.
/// Serialized on `account_id` so concurrent search/follow discovery cannot
/// leave duplicate unattached media rows behind.
pub async fn upsert_owncast(
    pool: &PgPool,
    account_id: i64,
    homepage_url: &str,
    status_url: &str,
    hls_master_url: &str,
) -> Result<RemoteStreamSource, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", account_id)
        .execute(&mut *tx)
        .await?;
    let existing = sqlx::query_scalar!(
        "SELECT media_id FROM remote_stream_sources WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let media_id = existing.unwrap_or_else(id::next);
    if existing.is_some() {
        sqlx::query!(
            r#"
            UPDATE media_attachments
            SET remote_url = $2,
                thumbnail_remote_url = $3,
                hls_master_url = $4,
                content_type = 'application/x-mpegURL',
                download_on_demand = true,
                live_permanent = true,
                processing = 'complete'
            WHERE id = $1
            "#,
            media_id,
            homepage_url,
            format!("{homepage_url}thumbnail.jpg"),
            hls_master_url,
        )
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query!(
            r#"
            INSERT INTO media_attachments
                (id, account_id, remote_url, content_type, description,
                 thumbnail_remote_url, processing, download_on_demand,
                 live_state, live_permanent, hls_master_url)
            VALUES ($1, $2, $3, 'application/x-mpegURL', NULL, $4, 'complete',
                    true, 'ended', true, $5)
            "#,
            media_id,
            account_id,
            homepage_url,
            format!("{homepage_url}thumbnail.jpg"),
            hls_master_url,
        )
        .execute(&mut *tx)
        .await?;
    }
    let source = sqlx::query_as!(
        RemoteStreamSource,
        r#"
        INSERT INTO remote_stream_sources
            (account_id, media_id, provider, homepage_url, status_url,
             hls_master_url)
        VALUES ($1, $2, 'owncast', $3, $4, $5)
        ON CONFLICT (account_id) DO UPDATE SET
            provider = 'owncast',
            homepage_url = EXCLUDED.homepage_url,
            status_url = EXCLUDED.status_url,
            hls_master_url = EXCLUDED.hls_master_url,
            updated_at = now()
        RETURNING account_id, media_id, provider, homepage_url, status_url,
                  hls_master_url, created_at, updated_at
        "#,
        account_id,
        media_id,
        homepage_url,
        status_url,
        hls_master_url,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(source)
}

pub async fn find_by_account(
    pool: &PgPool,
    account_id: i64,
) -> Result<Option<RemoteStreamSource>, DbError> {
    Ok(sqlx::query_as!(
        RemoteStreamSource,
        r#"SELECT account_id, media_id, provider, homepage_url, status_url,
                  hls_master_url, created_at, updated_at
           FROM remote_stream_sources WHERE account_id = $1"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn find_by_media(
    pool: &PgPool,
    media_id: i64,
) -> Result<Option<RemoteStreamSource>, DbError> {
    Ok(sqlx::query_as!(
        RemoteStreamSource,
        r#"SELECT account_id, media_id, provider, homepage_url, status_url,
                  hls_master_url, created_at, updated_at
           FROM remote_stream_sources WHERE media_id = $1"#,
        media_id,
    )
    .fetch_optional(pool)
    .await?)
}

pub async fn find_by_homepage(
    pool: &PgPool,
    homepage_url: &str,
) -> Result<Option<RemoteStreamSource>, DbError> {
    Ok(sqlx::query_as!(
        RemoteStreamSource,
        r#"SELECT account_id, media_id, provider, homepage_url, status_url,
                  hls_master_url, created_at, updated_at
           FROM remote_stream_sources WHERE homepage_url = $1"#,
        homepage_url,
    )
    .fetch_optional(pool)
    .await?)
}

/// Updates a source/status live player from `Owncast`'s authoritative status.
pub async fn update_media_state(
    pool: &PgPool,
    media_id: i64,
    state: &str,
    permanent: bool,
    title: Option<&str>,
    thumbnail_url: Option<&str>,
    hls_master_url: &str,
) -> Result<bool, DbError> {
    let changed = sqlx::query_scalar!(
        r#"
        UPDATE media_attachments
        SET live_state = $2,
            live_permanent = $3,
            description = COALESCE(NULLIF($4::text, ''), description),
            thumbnail_remote_url = COALESCE($5::text, thumbnail_remote_url),
            hls_master_url = $6
        WHERE id = $1
          AND (live_state IS DISTINCT FROM $2
               OR live_permanent IS DISTINCT FROM $3
               OR (NULLIF($4::text, '') IS NOT NULL
                   AND description IS DISTINCT FROM NULLIF($4::text, ''))
               OR ($5::text IS NOT NULL AND thumbnail_remote_url IS DISTINCT FROM $5)
               OR hls_master_url IS DISTINCT FROM $6)
        RETURNING id
        "#,
        media_id,
        state,
        permanent,
        title,
        thumbnail_url,
        hls_master_url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(changed.is_some())
}

/// Ends every older `Owncast` Note player when a new go-live Note arrives.
pub async fn end_prior_status_media(
    pool: &PgPool,
    account_id: i64,
    except_status_id: i64,
) -> Result<u64, DbError> {
    let done = sqlx::query!(
        r#"
        UPDATE media_attachments
        SET live_state = 'ended', live_permanent = false
        WHERE account_id = $1 AND status_id IS NOT NULL AND status_id <> $2
          AND live_state IS NOT NULL
        "#,
        account_id,
        except_status_id,
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Whether the account already has a federated go-live Note representing its
/// current session. Used to avoid showing the same player twice on a profile.
pub async fn has_live_status_media(pool: &PgPool, account_id: i64) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM media_attachments
            WHERE account_id = $1 AND status_id IS NOT NULL
              AND live_state = 'live'
        ) AS "exists!"
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?)
}

/// Loads the account-level media row for rendering.
pub async fn media(
    pool: &PgPool,
    source: &RemoteStreamSource,
) -> Result<Option<media::Media>, DbError> {
    Ok(media::find_by_ids(pool, &[source.media_id])
        .await?
        .into_iter()
        .next())
}
