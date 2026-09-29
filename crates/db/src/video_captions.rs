//! Language-specific captions associated with a video attachment.

use std::collections::HashMap;

use sqlx::{FromRow, PgConnection, PgExecutor, PgPool};

use crate::DbError;

#[derive(Debug, Clone, FromRow)]
pub struct Caption {
    pub media_id: i64,
    pub language: String,
    pub vtt_body: Option<String>,
    pub remote_url: Option<String>,
    pub automatically_generated: bool,
    pub legacy: bool,
}

#[must_use]
pub fn valid_language(language: &str) -> bool {
    !language.is_empty()
        && language.len() <= 35
        && language.split('-').enumerate().all(|(index, part)| {
            (1..=8).contains(&part.len())
                && if index == 0 {
                    part.len() >= 2 && part.bytes().all(|byte| byte.is_ascii_alphabetic())
                } else {
                    part.bytes().all(|byte| byte.is_ascii_alphanumeric())
                }
        })
}

pub async fn for_media<'e, E: PgExecutor<'e>>(
    executor: E,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<Caption>>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, Caption>(
        "SELECT c.media_id, c.language, c.vtt_body, c.remote_url,
                c.automatically_generated, false AS legacy
         FROM video_captions c WHERE c.media_id = ANY($1)
         UNION ALL
         SELECT m.id AS media_id,
                COALESCE(s.language, d.language, 'und') AS language,
                m.caption_vtt AS vtt_body, NULL::text AS remote_url,
                false AS automatically_generated, true AS legacy
         FROM media_attachments m
         LEFT JOIN statuses s ON s.id = m.status_id
         LEFT JOIN video_drafts d ON d.media_id = m.id
         WHERE m.id = ANY($1) AND m.caption_vtt IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM video_captions c WHERE c.media_id = m.id
                           AND lower(c.language) = lower(COALESCE(s.language, d.language, 'und')))
         ORDER BY media_id, language",
    )
    .bind(media_ids)
    .fetch_all(executor)
    .await?;
    let mut out: HashMap<i64, Vec<Caption>> = HashMap::new();
    for row in rows {
        out.entry(row.media_id).or_default().push(row);
    }
    Ok(out)
}

pub async fn replace_remote(
    pool: &PgPool,
    media_id: i64,
    tracks: &[Caption],
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM video_captions WHERE media_id = $1 AND remote_url IS NOT NULL")
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    for track in tracks {
        sqlx::query(
            "INSERT INTO video_captions
             (media_id, language, remote_url, automatically_generated)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (media_id, language) DO NOTHING",
        )
        .bind(media_id)
        .bind(&track.language)
        .bind(&track.remote_url)
        .bind(track.automatically_generated)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// An attachment and its author/channel must still be public and active when
/// this is served. We never expose a saved remote caption URL directly.
#[derive(Debug, FromRow)]
pub struct CaptionSource {
    pub vtt_body: Option<String>,
    pub remote_url: Option<String>,
}

pub async fn public_source(
    pool: &PgPool,
    media_id: i64,
    language: &str,
) -> Result<Option<CaptionSource>, DbError> {
    Ok(sqlx::query_as::<_, CaptionSource>(
        "SELECT c.vtt_body, c.remote_url FROM video_captions c
         JOIN media_attachments m ON m.id = c.media_id
         JOIN statuses s ON s.id = m.status_id
         JOIN accounts a ON a.id = m.account_id
         LEFT JOIN video_publications v ON v.media_id = m.id AND v.status_id = s.id
         WHERE c.media_id = $1 AND c.language = $2
           AND s.visibility IN ('public', 'unlisted')
           AND s.deleted_at IS NULL AND a.suspended_at IS NULL
           AND (v.media_id IS NULL OR EXISTS (
               SELECT 1 FROM public_native_videos p WHERE p.media_id = m.id))
           AND NOT EXISTS (SELECT 1 FROM remote_video_channels rv
               JOIN accounts rc ON rc.id = rv.channel_id
               WHERE rv.status_id = s.id AND (rc.suspended_at IS NOT NULL OR rc.deleted_at IS NOT NULL))",
    )
    .bind(media_id)
    .bind(language)
    .fetch_optional(pool)
    .await?)
}

/// Draft captions are private even if their media id can be guessed.
pub async fn draft_body(
    pool: &PgPool,
    media_id: i64,
    language: &str,
    owner: i64,
) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT c.vtt_body FROM video_captions c
         JOIN video_drafts d ON d.media_id = c.media_id
         JOIN media_attachments m ON m.id = c.media_id
         JOIN accounts a ON a.id = d.account_id
         JOIN accounts ch ON ch.id = d.channel_id
         WHERE c.media_id = $1 AND c.language = $2 AND d.account_id = $3
           AND m.account_id = $3 AND m.status_id IS NULL
           AND d.state = 'ready' AND d.expires_at > clock_timestamp()
           AND a.suspended_at IS NULL AND ch.suspended_at IS NULL",
    )
    .bind(media_id)
    .bind(language)
    .bind(owner)
    .fetch_optional(pool)
    .await?)
}

pub async fn upsert_draft(
    pool: &PgPool,
    media_id: i64,
    owner: i64,
    language: &str,
    body: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let authorized = sqlx::query_scalar::<_, i64>(
        "SELECT m.id FROM media_attachments m
         JOIN video_drafts d ON d.media_id = m.id
         JOIN accounts a ON a.id = d.account_id
         JOIN accounts ch ON ch.id = d.channel_id
         WHERE m.id = $1 AND m.account_id = $2 AND m.status_id IS NULL
           AND d.account_id = $2 AND d.state = 'ready'
           AND d.expires_at > clock_timestamp()
           AND a.suspended_at IS NULL AND ch.suspended_at IS NULL
         FOR UPDATE OF m",
    )
    .bind(media_id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    if authorized.is_none() {
        return Ok(false);
    }
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT (SELECT count(*) FROM video_captions WHERE media_id = m.id)
                + CASE WHEN m.caption_vtt IS NOT NULL
                         AND lower(COALESCE(d.language, 'und')) <> lower($2)
                       THEN 1 ELSE 0 END
         FROM media_attachments m JOIN video_drafts d ON d.media_id = m.id
         WHERE m.id = $1",
    )
    .bind(media_id)
    .bind(language)
    .fetch_one(&mut *tx)
    .await?;
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT media_id FROM video_captions WHERE media_id = $1 AND language = $2",
    )
    .bind(media_id)
    .bind(language)
    .fetch_optional(&mut *tx)
    .await?
    .is_some();
    if count >= 16 && !exists {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO video_captions (media_id, language, vtt_body)
         VALUES ($1, $2, $3)
         ON CONFLICT (media_id, language) DO UPDATE SET
             vtt_body = EXCLUDED.vtt_body, remote_url = NULL,
             automatically_generated = false",
    )
    .bind(media_id)
    .bind(language)
    .bind(body)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE media_attachments m SET caption_vtt = NULL
         FROM video_drafts d WHERE m.id = $1 AND d.media_id = m.id
           AND m.caption_vtt IS NOT NULL
           AND lower(COALESCE(d.language, 'und')) = lower($2)",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn remove_draft(
    pool: &PgPool,
    media_id: i64,
    owner: i64,
    language: &str,
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let authorized = sqlx::query_scalar::<_, i64>(
        "SELECT m.id FROM media_attachments m JOIN video_drafts d ON d.media_id = m.id
         WHERE m.id = $1 AND m.account_id = $2 AND d.account_id = $2
           AND m.status_id IS NULL AND d.state = 'ready'
           AND d.expires_at > clock_timestamp() FOR UPDATE OF m",
    )
    .bind(media_id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    if authorized.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "DELETE FROM video_captions WHERE media_id = $1 AND language = $2
           AND remote_url IS NULL",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE media_attachments m SET caption_vtt = NULL
         FROM video_drafts d WHERE m.id = $1 AND d.media_id = m.id
           AND m.caption_vtt IS NOT NULL
           AND lower(COALESCE(d.language, 'und')) = lower($2)",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

#[derive(Debug, Clone, FromRow)]
pub struct OwnedPublication {
    pub media_id: i64,
    pub status_id: i64,
    pub actor_id: i64,
    pub title: String,
}

pub async fn recent_owned_publications(
    pool: &PgPool,
    owner: i64,
) -> Result<Vec<OwnedPublication>, DbError> {
    Ok(sqlx::query_as::<_, OwnedPublication>(
        "SELECT v.media_id, v.status_id, s.account_id AS actor_id,
                COALESCE(s.title, 'Video') AS title
         FROM video_publications v
         JOIN video_channels ch ON ch.account_id = v.channel_id
         JOIN statuses s ON s.id = v.status_id
         JOIN media_attachments m ON m.id = v.media_id
         JOIN accounts ca ON ca.id=ch.account_id
         JOIN accounts o ON o.id=ch.owner_account_id
         WHERE ca.suspended_at IS NULL AND o.suspended_at IS NULL
           AND (ch.owner_account_id=$1 OR EXISTS(SELECT 1 FROM video_channel_grants g
                WHERE g.channel_id=ch.account_id AND g.account_id=$1 AND g.accepted
                  AND (g.role='editor' OR s.account_id=$1)))
           AND s.deleted_at IS NULL AND s.visibility IN ('public', 'unlisted')
         ORDER BY s.created_at DESC LIMIT 30",
    )
    .bind(owner)
    .fetch_all(pool)
    .await?)
}

pub async fn owned_publication(
    pool: &PgPool,
    media_id: i64,
    owner: i64,
) -> Result<Option<OwnedPublication>, DbError> {
    let mut tx = pool.begin().await?;
    let publication = lock_owned_publication(&mut tx, media_id, owner).await?;
    tx.commit().await?;
    Ok(publication)
}

/// Call from a transaction and retain the lock across caption mutation,
/// `Update(Video)` creation and commit.
pub async fn lock_owned_publication(
    conn: &mut PgConnection,
    media_id: i64,
    owner: i64,
) -> Result<Option<OwnedPublication>, DbError> {
    if !crate::video_channel_access::lock_publication(conn, media_id, owner).await? {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, OwnedPublication>(
        "SELECT v.media_id, v.status_id, s.account_id AS actor_id,
                COALESCE(s.title, 'Video') AS title
         FROM video_publications v
         JOIN video_channels ch ON ch.account_id = v.channel_id
         JOIN statuses s ON s.id = v.status_id
         JOIN media_attachments m ON m.id = v.media_id
         WHERE v.media_id = $1 AND s.deleted_at IS NULL
           AND s.visibility IN ('public', 'unlisted')
         FOR UPDATE OF m, s",
    )
    .bind(media_id)
    .fetch_optional(conn)
    .await?)
}

/// Caller must hold `lock_owned_publication` on the same connection.
pub async fn upsert_published(
    conn: &mut PgConnection,
    media_id: i64,
    language: &str,
    body: &str,
) -> Result<bool, DbError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT (SELECT count(*) FROM video_captions WHERE media_id = m.id)
                + CASE WHEN m.caption_vtt IS NOT NULL
                         AND lower(COALESCE(s.language, 'und')) <> lower($2)
                       THEN 1 ELSE 0 END
         FROM media_attachments m JOIN statuses s ON s.id = m.status_id
         WHERE m.id = $1",
    )
    .bind(media_id)
    .bind(language)
    .fetch_one(&mut *conn)
    .await?;
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT media_id FROM video_captions WHERE media_id = $1 AND language = $2",
    )
    .bind(media_id)
    .bind(language)
    .fetch_optional(&mut *conn)
    .await?
    .is_some();
    if count >= 16 && !exists {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO video_captions (media_id, language, vtt_body)
         VALUES ($1, $2, $3)
         ON CONFLICT (media_id, language) DO UPDATE SET
             vtt_body = EXCLUDED.vtt_body, remote_url = NULL,
             automatically_generated = false",
    )
    .bind(media_id)
    .bind(language)
    .bind(body)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE media_attachments m SET caption_vtt = NULL
         FROM statuses s WHERE m.id = $1 AND s.id = m.status_id
           AND m.caption_vtt IS NOT NULL
           AND lower(COALESCE(s.language, 'und')) = lower($2)",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// Caller must hold `lock_owned_publication` on the same connection.
pub async fn remove_published(
    conn: &mut PgConnection,
    media_id: i64,
    language: &str,
) -> Result<bool, DbError> {
    let removed = sqlx::query(
        "DELETE FROM video_captions WHERE media_id = $1 AND language = $2
           AND remote_url IS NULL",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *conn)
    .await?;
    let cleared = sqlx::query(
        "UPDATE media_attachments m SET caption_vtt = NULL
         FROM statuses s WHERE m.id = $1 AND s.id = m.status_id
           AND m.caption_vtt IS NOT NULL
           AND lower(COALESCE(s.language, 'und')) = lower($2)",
    )
    .bind(media_id)
    .bind(language)
    .execute(&mut *conn)
    .await?;
    Ok(removed.rows_affected() + cleared.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::valid_language;

    #[test]
    fn caption_languages_are_safe_url_segments() {
        for language in ["en", "fr-CA", "zh-Hant-TW", "und"] {
            assert!(valid_language(language), "{language}");
        }
        for language in ["", "e", "../fr", "fr/CA", "fr..CA", "é", "en-", "en--US"] {
            assert!(!valid_language(language), "{language}");
        }
    }
}
