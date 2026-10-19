//! Small presentation sidecars; none confer discovery eligibility.
use super::{Lease, PgPool};
use crate::DbError;

pub async fn create_page_receipt(
    pool: &PgPool,
    viewer: i64,
    token: &str,
    entries: &[i64],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM accounts WHERE id=$1 FOR UPDATE")
        .bind(viewer)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM discovery_page_receipts WHERE created_at<now()-interval '2 hours' OR (account_id=$1 AND token IN (SELECT token FROM discovery_page_receipts WHERE account_id=$1 ORDER BY created_at DESC OFFSET 31))").bind(viewer).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO discovery_page_receipts(token,account_id,entries) VALUES($1,$2,$3)")
        .bind(token)
        .bind(viewer)
        .bind(entries)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn consume_page_receipt(pool: &PgPool, viewer: i64, token: &str) -> Result<(), DbError> {
    let ids: Option<Vec<i64>> = sqlx::query_scalar("DELETE FROM discovery_page_receipts WHERE token=$1 AND account_id=$2 AND created_at>now()-interval '2 hours' RETURNING entries").bind(token).bind(viewer).fetch_optional(pool).await?;
    if let Some(ids) = ids {
        for chunk in ids.chunks(50) {
            super::remember_seen(pool, viewer, chunk, false).await?;
        }
    }
    Ok(())
}
pub async fn save_actor_label(
    pool: &PgPool,
    lease: &Lease,
    uri: &str,
    username: &str,
) -> Result<(), DbError> {
    if username.is_empty()
        || username.len() > 255
        || username
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '@')
    {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if super::valid_lease(&mut tx, &settings, lease).await? {
        sqlx::query("INSERT INTO discovery_actor_labels(uri,username) SELECT $1,$2 WHERE EXISTS(SELECT 1 FROM discovery_actor_labels WHERE uri=$1) OR (SELECT count(*) FROM discovery_actor_labels)<$3 ON CONFLICT(uri) DO UPDATE SET username=excluded.username,updated_at=now()")
            .bind(uri).bind(username).bind(i64::from(settings.max_entries)*3).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn actor_labels(
    pool: &PgPool,
    uris: &[String],
    viewer: i64,
) -> Result<Vec<(String, String, Option<String>)>, DbError> {
    Ok(sqlx::query_as("SELECT u.uri,COALESCE(a.username,l.username),a.domain FROM unnest($1::text[]) AS u(uri) LEFT JOIN accounts a ON a.uri=u.uri LEFT JOIN discovery_actor_labels l ON l.uri=u.uri WHERE COALESCE(a.username,l.username) IS NOT NULL AND (a.id IS NULL OR (a.suspended_at IS NULL AND a.silenced_at IS NULL AND NOT account_hidden($2,a.id)))")
        .bind(uris).bind(viewer).fetch_all(pool).await?)
}
#[derive(sqlx::FromRow)]
pub struct ServerIcon {
    pub icon_uri: Option<String>,
    pub image: Option<Vec<u8>>,
    pub due: bool,
}
pub async fn server_icon(pool: &PgPool, source: i64) -> Result<Option<ServerIcon>, DbError> {
    Ok(sqlx::query_as("SELECT icon_uri,image,next_check_at<=now() AS due FROM discovery_server_icons WHERE source_id=$1").bind(source).fetch_optional(pool).await?)
}
pub async fn save_server_icon(
    pool: &PgPool,
    lease: &Lease,
    uri: Option<&str>,
    image: Option<&[u8]>,
    outcome: &str,
    retry_hours: i32,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if super::valid_lease(&mut tx, &settings, lease).await? {
        sqlx::query("INSERT INTO discovery_server_icons(source_id,icon_uri,image,outcome,next_check_at) VALUES($1,$2,$3,$4,now()+$5*interval '1 hour') ON CONFLICT(source_id) DO UPDATE SET icon_uri=excluded.icon_uri,image=COALESCE(excluded.image,discovery_server_icons.image),outcome=excluded.outcome,next_check_at=excluded.next_check_at")
            .bind(lease.source_id).bind(uri).bind(image).bind(outcome).bind(retry_hours).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
/// An origin may advertise its icon on a CDN. Only this retained icon origin is
/// an additional destination, with the same hard per-origin/global accounting.
pub async fn request_origin_allowed(
    pool: &PgPool,
    lease: &Lease,
    uri: &str,
) -> Result<bool, DbError> {
    let origin = super::address(uri)?.1;
    if origin == super::address(&lease.target_uri)?.1 {
        return Ok(true);
    }
    let icon: Option<String> =
        sqlx::query_scalar("SELECT icon_uri FROM discovery_server_icons WHERE source_id=$1")
            .bind(lease.source_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(icon.is_some_and(|u| super::address(&u).is_ok_and(|(_, o)| o == origin)))
}

pub async fn schedule_presentation(pool: &PgPool) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if !settings.runs("metadata_cache") || settings.paused {
        return Ok(());
    }
    // An ordinary job may have populated an icon before the dedicated job ran.
    sqlx::query("DELETE FROM discovery_jobs j USING discovery_server_icons i WHERE j.operation='refresh_presentation' AND j.source_id=i.source_id AND j.lease_token IS NULL AND i.next_check_at>now()")
        .execute(&mut *tx).await?;
    let sources:Vec<(i64,String,Option<i64>)>=sqlx::query_as(
        "SELECT s.id,s.endpoint,s.introduced_by FROM discovery_sources s LEFT JOIN discovery_server_icons i ON i.source_id=s.id
         WHERE s.enabled AND s.verified_at IS NOT NULL AND s.capabilities && ARRAY['directory','community_directory','video_catalogue']::text[]
           AND (i.source_id IS NULL OR i.next_check_at<=now())
           AND NOT EXISTS(SELECT 1 FROM discovery_jobs j WHERE j.source_id=s.id AND j.operation='refresh_presentation')
           ORDER BY (i.image IS NULL) DESC,s.id LIMIT 64")
        .fetch_all(&mut *tx).await?;
    for (id, endpoint, introducer) in sources {
        super::enqueue_locked(
            &mut tx,
            &settings,
            id,
            &endpoint,
            super::Operation::RefreshPresentation,
            introducer,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn save_catalogue_languages(
    pool: &PgPool,
    lease: &Lease,
    values: &[(String, String)],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if settings.runs("metadata_cache") && super::valid_lease(&mut tx, &settings, lease).await? {
        for (uri, language) in values.iter().take(20) {
            sqlx::query("UPDATE discovery_entries SET language=$3 WHERE canonical_uri=$1 AND source_id=$2 AND kind='video' AND state='active' AND language IS NULL")
                .bind(uri).bind(lease.source_id).bind(language).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Reuse presentation fields in an already fetched, same-origin catalogue.
/// Only independently verified, active identities qualify; this cannot admit
/// content, extend retention, or establish consent from a directory hint.
pub async fn save_catalogue_labels(
    pool: &PgPool,
    lease: &Lease,
    values: &[(String, String)],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let settings = super::lock_settings(&mut tx).await?;
    if settings.runs("metadata_cache") && super::valid_lease(&mut tx, &settings, lease).await? {
        let origin = super::address(&lease.target_uri)?.1;
        for (uri, username) in values.iter().take(20) {
            if username.is_empty()
                || username.len() > 255
                || username
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || c == '@')
                || !super::address(uri).is_ok_and(|(_, o)| o == origin)
            {
                continue;
            }
            sqlx::query("INSERT INTO discovery_actor_labels(uri,username) SELECT e.canonical_uri,$3 FROM discovery_entries e WHERE e.canonical_uri=$1 AND e.source_id=$2 AND e.kind IN ('profile','community') AND e.state='active' AND e.valid_until>now() ON CONFLICT(uri) DO NOTHING")
                .bind(uri).bind(lease.source_id).bind(username).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
