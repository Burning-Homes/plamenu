//! Link preview cards (Mastodon's `preview_cards`): one row per fetched URL,
//! attached to statuses through `preview_cards_statuses`, plus the crawl
//! queue feeding the link-preview worker.

use std::collections::HashMap;

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PreviewCard {
    pub id: i64,
    /// Canonical URL (post-redirect / `<link rel="canonical">`).
    pub url: String,
    pub title: String,
    pub description: String,
    /// `link` | `photo` | `video` | `audio`.
    pub kind: String,
    pub author_name: String,
    pub author_url: String,
    pub provider_name: String,
    pub provider_url: String,
    /// Sanitized embed HTML.
    pub html: String,
    pub width: i32,
    pub height: i32,
    /// Remote image origin URL (metadata / the direct-fallback target).
    pub image_url: Option<String>,
    /// Our locally-cached copy of the image, served from `/media/`. `None`
    /// until the media proxy fetches it on first view.
    pub image_file_name: Option<String>,
    pub image_content_type: Option<String>,
    pub image_file_size: Option<i64>,
    pub image_cached_at: Option<OffsetDateTime>,
    pub image_description: String,
    pub embed_url: String,
    pub language: Option<String>,
    pub published_at: Option<OffsetDateTime>,
    /// The verified `fediverse:creator` author (Mastodon's
    /// `author_account_id`): set only when that account's attribution
    /// domains authorize the card's domain.
    pub author_account_id: Option<i64>,
    /// Last successful fetch.
    pub updated_at: OffsetDateTime,
}

/// Everything extracted from a fetched page, ready to store.
#[derive(Debug, Default)]
pub struct NewPreviewCard<'a> {
    pub url: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub kind: &'a str,
    pub author_name: &'a str,
    pub author_url: &'a str,
    pub provider_name: &'a str,
    pub provider_url: &'a str,
    pub html: &'a str,
    pub width: i32,
    pub height: i32,
    pub image_url: Option<&'a str>,
    pub image_description: &'a str,
    pub embed_url: &'a str,
    pub language: Option<&'a str>,
    pub published_at: Option<OffsetDateTime>,
    pub author_account_id: Option<i64>,
}

const COLS: &str = "id, url, title, description, kind, author_name, author_url, \
                    provider_name, provider_url, html, width, height, image_url, \
                    image_file_name, image_content_type, image_file_size, \
                    image_cached_at, image_description, embed_url, language, \
                    published_at, author_account_id, updated_at";
const _: &str = COLS; // documentation: every query selects exactly these

/// Inserts or refreshes the card for `new.url` (cards are keyed by URL and
/// shared across statuses; a re-fetch replaces every field).
pub async fn upsert(pool: &PgPool, new: NewPreviewCard<'_>) -> Result<PreviewCard, DbError> {
    let card = sqlx::query_as!(
        PreviewCard,
        r#"
        INSERT INTO preview_cards (id, url, title, description, kind, author_name,
                                   author_url, provider_name, provider_url, html,
                                   width, height, image_url, image_description,
                                   embed_url, language, published_at,
                                   author_account_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        ON CONFLICT (url) DO UPDATE SET
            title = EXCLUDED.title,
            description = EXCLUDED.description,
            kind = EXCLUDED.kind,
            author_name = EXCLUDED.author_name,
            author_url = EXCLUDED.author_url,
            provider_name = EXCLUDED.provider_name,
            provider_url = EXCLUDED.provider_url,
            html = EXCLUDED.html,
            width = EXCLUDED.width,
            height = EXCLUDED.height,
            image_url = EXCLUDED.image_url,
            -- A changed origin image invalidates the cached copy; a re-crawl
            -- that kept the same image_url keeps ours.
            image_file_name = CASE
                WHEN preview_cards.image_url IS DISTINCT FROM EXCLUDED.image_url
                THEN NULL ELSE preview_cards.image_file_name END,
            image_content_type = CASE
                WHEN preview_cards.image_url IS DISTINCT FROM EXCLUDED.image_url
                THEN NULL ELSE preview_cards.image_content_type END,
            image_file_size = CASE
                WHEN preview_cards.image_url IS DISTINCT FROM EXCLUDED.image_url
                THEN NULL ELSE preview_cards.image_file_size END,
            image_cached_at = CASE
                WHEN preview_cards.image_url IS DISTINCT FROM EXCLUDED.image_url
                THEN NULL ELSE preview_cards.image_cached_at END,
            image_description = EXCLUDED.image_description,
            embed_url = EXCLUDED.embed_url,
            language = EXCLUDED.language,
            published_at = EXCLUDED.published_at,
            author_account_id = EXCLUDED.author_account_id,
            updated_at = now()
        RETURNING id, url, title, description, kind, author_name, author_url,
                  provider_name, provider_url, html, width, height, image_url,
                  image_file_name, image_content_type, image_file_size,
                  image_cached_at, image_description, embed_url, language,
                  published_at, author_account_id, updated_at
        "#,
        id::next(),
        new.url,
        new.title,
        new.description,
        new.kind,
        new.author_name,
        new.author_url,
        new.provider_name,
        new.provider_url,
        new.html,
        new.width,
        new.height,
        new.image_url,
        new.image_description,
        new.embed_url,
        new.language,
        new.published_at,
        new.author_account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(card)
}

/// The stored card for a URL, if any.
pub async fn find_by_url(pool: &PgPool, url: &str) -> Result<Option<PreviewCard>, DbError> {
    let card = sqlx::query_as!(
        PreviewCard,
        r#"
        SELECT id, url, title, description, kind, author_name, author_url,
               provider_name, provider_url, html, width, height, image_url,
               image_file_name, image_content_type, image_file_size,
               image_cached_at, image_description, embed_url, language,
               published_at, author_account_id, updated_at
        FROM preview_cards WHERE url = $1
        "#,
        url,
    )
    .fetch_optional(pool)
    .await?;
    Ok(card)
}

/// A card by its id.
pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<PreviewCard>, DbError> {
    let card = sqlx::query_as!(
        PreviewCard,
        r#"
        SELECT id, url, title, description, kind, author_name, author_url,
               provider_name, provider_url, html, width, height, image_url,
               image_file_name, image_content_type, image_file_size,
               image_cached_at, image_description, embed_url, language,
               published_at, author_account_id, updated_at
        FROM preview_cards WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(card)
}

/// Records the locally-cached copy of a card's image after the media proxy
/// fetched it. `file_size` is the stored byte count.
pub async fn set_image_file(
    pool: &PgPool,
    id: i64,
    file_name: &str,
    content_type: &str,
    file_size: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE preview_cards
        SET image_file_name = $2, image_content_type = $3,
            image_file_size = $4, image_cached_at = now()
        WHERE id = $1
        "#,
        id,
        file_name,
        content_type,
        file_size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Evicts cached card images older than the retention period, returning the
/// freed file names (the media proxy refetches on demand).
pub async fn evict_cached_images(
    pool: &PgPool,
    retention: std::time::Duration,
    limit: i64,
) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        r#"
        WITH due AS (
            SELECT id, image_file_name
            FROM preview_cards
            WHERE image_file_name IS NOT NULL
              AND image_cached_at IS NOT NULL
              AND image_cached_at < now() - ($1 * interval '1 second')
            ORDER BY image_cached_at
            LIMIT $2
        )
        UPDATE preview_cards c
        SET image_file_name = NULL, image_file_size = NULL, image_cached_at = NULL
        FROM due
        WHERE c.id = due.id
        RETURNING due.image_file_name AS "image_file_name!"
        "#,
        retention.as_secs_f64(),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(files)
}

/// Attaches a card to a status, remembering the link as it appeared in the
/// status (`original_url`). Idempotent.
pub async fn attach(
    pool: &PgPool,
    status_id: i64,
    preview_card_id: i64,
    original_url: &str,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO preview_cards_statuses (preview_card_id, status_id, url)
        VALUES ($1, $2, $3)
        ON CONFLICT DO NOTHING
        "#,
        preview_card_id,
        status_id,
        original_url,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether a status already has a card attached.
pub async fn exists_for_status(pool: &PgPool, status_id: i64) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM preview_cards_statuses WHERE status_id = $1 LIMIT 1"#,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Detaches every card from a status (edits re-crawl from scratch). The
/// card rows stay — they are shared and re-fetched on a two-week cadence.
pub async fn detach_all<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM preview_cards_statuses WHERE status_id = $1",
        status_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The cards of a batch of statuses, each with the status' original link
/// URL, keyed by status id.
pub async fn for_statuses(
    pool: &PgPool,
    status_ids: &[i64],
) -> Result<HashMap<i64, (PreviewCard, String)>, DbError> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        r#"
        SELECT s.status_id, s.url AS original_url,
               c.id, c.url, c.title, c.description, c.kind, c.author_name,
               c.author_url, c.provider_name, c.provider_url, c.html, c.width,
               c.height, c.image_url, c.image_file_name, c.image_content_type,
               c.image_file_size, c.image_cached_at, c.image_description,
               c.embed_url, c.language, c.published_at, c.author_account_id,
               c.updated_at
        FROM preview_cards_statuses s
        JOIN preview_cards c ON c.id = s.preview_card_id
        WHERE s.status_id = ANY($1)
        "#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.status_id,
                (
                    PreviewCard {
                        id: row.id,
                        url: row.url,
                        title: row.title,
                        description: row.description,
                        kind: row.kind,
                        author_name: row.author_name,
                        author_url: row.author_url,
                        provider_name: row.provider_name,
                        provider_url: row.provider_url,
                        html: row.html,
                        width: row.width,
                        height: row.height,
                        image_url: row.image_url,
                        image_file_name: row.image_file_name,
                        image_content_type: row.image_content_type,
                        image_file_size: row.image_file_size,
                        image_cached_at: row.image_cached_at,
                        image_description: row.image_description,
                        embed_url: row.embed_url,
                        language: row.language,
                        published_at: row.published_at,
                        author_account_id: row.author_account_id,
                        updated_at: row.updated_at,
                    },
                    row.original_url,
                ),
            )
        })
        .collect())
}

/// How long a claimed crawl stays invisible before a crashed worker's job
/// becomes due again.
const CRAWL_LEASE_SECONDS: f64 = 600.0;

/// Drop a crawl once it has been reclaimed by lease expiry this many times —
/// a process that keeps crashing on the same job must not loop forever. The
/// crawl is otherwise single-attempt (like Mastodon's link-crawl job); only
/// crash reclaims bump `attempts`.
const MAX_CRAWL_ATTEMPTS: i32 = 5;

/// A leased link-crawl job: its own id (to [`complete_crawl`] it) and the
/// status whose links to crawl.
#[derive(Debug, Clone, Copy)]
pub struct ClaimedCrawl {
    pub id: i64,
    pub status_id: i64,
    /// Times claimed, including this one; > [`MAX_CRAWL_ATTEMPTS`] means give up.
    pub attempts: i32,
}

impl ClaimedCrawl {
    /// Whether this job has outlived too many process crashes to keep retrying.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.attempts > MAX_CRAWL_ATTEMPTS
    }
}

/// Enqueues a status for link crawling.
/// Executor-generic so it can run inside the status-creation transaction.
pub async fn enqueue_crawl<'e, E: PgExecutor<'e>>(
    executor: E,
    status_id: i64,
) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO link_crawl_jobs (id, status_id) VALUES ($1, $2)",
        id::next(),
        status_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Claims up to `limit` due crawl jobs, leasing each for
/// [`CRAWL_LEASE_SECONDS`] and bumping its attempt counter.
/// Crawling is single-attempt and best-effort like Mastodon's link-crawl job,
/// but the row now survives until the worker [`complete_crawl`]s it, so a crash
/// between claim and completion lets the lease expire and the crawl runs again
/// instead of being silently lost.
pub async fn claim_due_crawls(pool: &PgPool, limit: i64) -> Result<Vec<ClaimedCrawl>, DbError> {
    let jobs = sqlx::query_as!(
        ClaimedCrawl,
        r#"
        UPDATE link_crawl_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM link_crawl_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, status_id, attempts
        "#,
        limit,
        CRAWL_LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a finished (or exhausted) crawl job.
pub async fn complete_crawl(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM link_crawl_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Number of queued crawl jobs (diagnostics / tests).
pub async fn pending_crawl_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM link_crawl_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Test hook: backdates a card's last fetch so refresh paths can be tested.
pub async fn backdate_updated_at(
    pool: &PgPool,
    card_id: i64,
    updated_at: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE preview_cards SET updated_at = $2 WHERE id = $1",
        card_id,
        updated_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}
