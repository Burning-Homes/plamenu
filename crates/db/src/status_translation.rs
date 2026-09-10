//! Persistent status-translation cache: one row
//! per (status, target language), so each pair is translated once per edit
//! ever — across restarts and across viewers.
//!
//! `source_hash` is a sha256 the server computes over the exact fragments it
//! would send to the backend. Readers compare it against the current
//! fragments: a mismatch (the status was edited) is a cache miss and the row
//! is overwritten by the next successful translation. Media descriptions are
//! stored as parallel arrays (`media_ids[i]` ↔ `media_descriptions[i]`).

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StatusTranslation {
    pub status_id: i64,
    pub target_language: String,
    pub source_hash: Vec<u8>,
    pub provider: String,
    pub detected_source_language: Option<String>,
    pub title: String,
    pub content: String,
    pub spoiler_text: String,
    pub poll_options: Vec<String>,
    pub media_ids: Vec<i64>,
    pub media_descriptions: Vec<String>,
    pub created_at: OffsetDateTime,
    pub last_used_at: OffsetDateTime,
}

#[derive(Debug)]
pub struct NewStatusTranslation<'a> {
    pub status_id: i64,
    pub target_language: &'a str,
    pub source_hash: &'a [u8],
    pub provider: &'a str,
    pub detected_source_language: Option<&'a str>,
    pub title: &'a str,
    pub content: &'a str,
    pub spoiler_text: &'a str,
    pub poll_options: &'a [String],
    pub media_ids: &'a [i64],
    pub media_descriptions: &'a [String],
}

/// Fetches the cached translation for `(status_id, target_language)`, bumping
/// `last_used_at` at most once per day (it drives retention, not correctness,
/// so hot rows don't churn writes on every read).
pub async fn find(
    pool: &PgPool,
    status_id: i64,
    target_language: &str,
) -> Result<Option<StatusTranslation>, DbError> {
    let row = sqlx::query_as!(
        StatusTranslation,
        r#"
        SELECT status_id, target_language, source_hash, provider,
               detected_source_language, title, content, spoiler_text,
               poll_options, media_ids, media_descriptions, created_at,
               last_used_at
        FROM status_translations
        WHERE status_id = $1 AND target_language = $2
        "#,
        status_id,
        target_language,
    )
    .fetch_optional(pool)
    .await?;
    if let Some(row) = &row
        && row.last_used_at < OffsetDateTime::now_utc() - time::Duration::days(1)
    {
        sqlx::query!(
            "UPDATE status_translations SET last_used_at = now()
             WHERE status_id = $1 AND target_language = $2",
            status_id,
            target_language,
        )
        .execute(pool)
        .await?;
    }
    Ok(row)
}

/// Stores (or replaces, e.g. after an edit changed the source hash) a
/// translation.
pub async fn upsert(pool: &PgPool, new: &NewStatusTranslation<'_>) -> Result<(), DbError> {
    crate::upsert_racing(|| {
        sqlx::query!(
            r#"
            INSERT INTO status_translations
                (status_id, target_language, source_hash, provider,
                 detected_source_language, title, content, spoiler_text,
                 poll_options, media_ids, media_descriptions)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (status_id, target_language) DO UPDATE
            SET source_hash = EXCLUDED.source_hash,
                provider = EXCLUDED.provider,
                detected_source_language = EXCLUDED.detected_source_language,
                title = EXCLUDED.title,
                content = EXCLUDED.content,
                spoiler_text = EXCLUDED.spoiler_text,
                poll_options = EXCLUDED.poll_options,
                media_ids = EXCLUDED.media_ids,
                media_descriptions = EXCLUDED.media_descriptions,
                created_at = now(),
                last_used_at = now()
            "#,
            new.status_id,
            new.target_language,
            new.source_hash,
            new.provider,
            new.detected_source_language,
            new.title,
            new.content,
            new.spoiler_text,
            new.poll_options,
            new.media_ids,
            new.media_descriptions,
        )
        .execute(pool)
    })
    .await?;
    Ok(())
}

/// Deletes rows unused for `retention_days`; `0` keeps them forever (only
/// status deletion or the row cap removes them then).
pub async fn prune_unused(pool: &PgPool, retention_days: i32) -> Result<u64, DbError> {
    if retention_days <= 0 {
        return Ok(0);
    }
    let result = sqlx::query!(
        "DELETE FROM status_translations
         WHERE last_used_at < now() - make_interval(days => $1)",
        retention_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Evicts least-recently-used rows beyond `max_rows`; `0` means uncapped.
pub async fn enforce_cap(pool: &PgPool, max_rows: i32) -> Result<u64, DbError> {
    if max_rows <= 0 {
        return Ok(0);
    }
    let result = sqlx::query!(
        r#"
        DELETE FROM status_translations
        WHERE (status_id, target_language) IN (
            SELECT status_id, target_language FROM status_translations
            ORDER BY last_used_at DESC
            OFFSET $1
        )
        "#,
        i64::from(max_rows),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

    async fn cached_status(pool: &PgPool) -> i64 {
        let account = account::create_local(
            pool,
            NewLocalAccount {
                username: "translated",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;
        status::create_local(
            pool,
            NewLocalStatus::new(account, "<p>hola</p>", "public", None),
        )
        .await
        .unwrap()
        .id
    }

    fn translation(status_id: i64) -> NewStatusTranslation<'static> {
        NewStatusTranslation {
            status_id,
            target_language: "en",
            source_hash: b"hash-v1",
            provider: "TestMT",
            detected_source_language: Some("es"),
            title: "Hello title",
            content: "<p>hello</p>",
            spoiler_text: "",
            poll_options: &[],
            media_ids: &[],
            media_descriptions: &[],
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_find_roundtrip_and_replace(pool: PgPool) {
        let status_id = cached_status(&pool).await;
        upsert(&pool, &translation(status_id)).await.unwrap();

        let row = find(&pool, status_id, "en").await.unwrap().unwrap();
        assert_eq!(row.content, "<p>hello</p>");
        assert_eq!(row.title, "Hello title");
        assert_eq!(row.source_hash, b"hash-v1");
        assert_eq!(row.detected_source_language.as_deref(), Some("es"));
        assert!(find(&pool, status_id, "de").await.unwrap().is_none());

        // Replacing after an edit swaps the hash and payload in place.
        let mut edited = translation(status_id);
        edited.source_hash = b"hash-v2";
        edited.content = "<p>hello again</p>";
        upsert(&pool, &edited).await.unwrap();
        let row = find(&pool, status_id, "en").await.unwrap().unwrap();
        assert_eq!(row.source_hash, b"hash-v2");
        assert_eq!(row.content, "<p>hello again</p>");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stale_reads_touch_last_used(pool: PgPool) {
        let status_id = cached_status(&pool).await;
        upsert(&pool, &translation(status_id)).await.unwrap();
        sqlx::query!("UPDATE status_translations SET last_used_at = now() - interval '3 days'",)
            .execute(&pool)
            .await
            .unwrap();

        find(&pool, status_id, "en").await.unwrap().unwrap();
        let row = find(&pool, status_id, "en").await.unwrap().unwrap();
        assert!(row.last_used_at > OffsetDateTime::now_utc() - time::Duration::hours(1));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn prune_and_cap(pool: PgPool) {
        let status_id = cached_status(&pool).await;
        upsert(&pool, &translation(status_id)).await.unwrap();
        let mut second = translation(status_id);
        second.target_language = "de";
        upsert(&pool, &second).await.unwrap();

        // Retention: nothing is old enough; then everything is.
        assert_eq!(prune_unused(&pool, 30).await.unwrap(), 0);
        assert_eq!(prune_unused(&pool, 0).await.unwrap(), 0);
        sqlx::query!(
            "UPDATE status_translations SET last_used_at = now() - interval '40 days'
             WHERE target_language = 'de'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(prune_unused(&pool, 30).await.unwrap(), 1);

        // Cap: uncapped keeps, cap of 1 keeps only the freshest row.
        upsert(&pool, &second).await.unwrap();
        assert_eq!(enforce_cap(&pool, 0).await.unwrap(), 0);
        assert_eq!(enforce_cap(&pool, 1).await.unwrap(), 1);
        assert!(find(&pool, status_id, "de").await.unwrap().is_some());
        assert!(find(&pool, status_id, "en").await.unwrap().is_none());
    }
}
