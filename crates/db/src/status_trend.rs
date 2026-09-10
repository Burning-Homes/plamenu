//! Trending-status ranking state — Mastodon's `status_trends` and the
//! `Trends::Statuses` engine's persistence. Unlike tags, a status's score comes
//! from its live engagement (favourites + reblogs) decayed by the post's age, so
//! this module also assembles the scoring candidates directly from the
//! interaction tables rather than a usage rollup.

use std::collections::HashSet;

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::DbError;
use crate::status::Status;

/// A status in scope for a refresh pass — it has a live trend row or saw
/// activity (creation, favourite or reblog) today. Carries the engagement
/// counts the score reads, the age basis (`created_at`), and whether the status
/// is currently eligible to trend at all.
#[derive(Debug, Clone)]
pub struct ScoreInput {
    pub status_id: i64,
    pub account_id: i64,
    pub created_at: OffsetDateTime,
    pub favourites: i64,
    pub reblogs: i64,
    pub trendable: Option<bool>,
    pub language: Option<String>,
    pub eligible: bool,
}

/// Every status to (re)score this pass: those with a live trend row unioned with
/// those created, favourited or reblogged today, each with its engagement counts
/// and a precomputed eligibility flag (Mastodon's `eligible?`: a public,
/// non-reply, non-sensitive original by a discoverable, unsilenced,
/// unsensitized, language-tagged account).
pub async fn score_inputs(pool: &PgPool, today: Date) -> Result<Vec<ScoreInput>, DbError> {
    let rows = sqlx::query_as!(
        ScoreInput,
        r#"
        SELECT s.id AS "status_id!", s.account_id AS "account_id!",
               s.created_at AS "created_at!",
               (SELECT count(*) FROM favourites f WHERE f.status_id = s.id) AS "favourites!",
               (SELECT count(*) FROM statuses r WHERE r.reblog_of_id = s.id) AS "reblogs!",
               s.trendable, s.language,
               (s.visibility = 'public' AND s.reblog_of_id IS NULL
                AND s.in_reply_to_id IS NULL AND s.sensitive = false
                AND (s.spoiler_text IS NULL OR s.spoiler_text = '')
                AND s.language IS NOT NULL
                -- discoverable is nullable; a bare AND would make the whole
                -- expression NULL and blow up the non-null decode assertion.
                AND a.discoverable IS TRUE AND NOT account_silenced(a.id)
                AND a.sensitized_at IS NULL
                AND s.deleted_at IS NULL -- STUBFILTER: a soft-deleted stub must score ineligible
                ) AS "eligible!"
        FROM statuses s
        JOIN accounts a ON a.id = s.account_id
        WHERE s.id IN (
            SELECT status_id FROM status_trends
            UNION
            SELECT id FROM statuses
                WHERE reblog_of_id IS NULL
                  AND date_trunc('day', created_at AT TIME ZONE 'UTC')::date = $1
            UNION
            SELECT status_id FROM favourites
                WHERE date_trunc('day', created_at AT TIME ZONE 'UTC')::date = $1
            UNION
            SELECT reblog_of_id FROM statuses
                WHERE reblog_of_id IS NOT NULL
                  AND date_trunc('day', created_at AT TIME ZONE 'UTC')::date = $1
        )
        "#,
        today,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Inserts or updates a status's trend row with its decayed score.
pub async fn upsert(
    pool: &PgPool,
    status_id: i64,
    account_id: i64,
    score: f64,
    language: Option<&str>,
    allowed: bool,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO status_trends (status_id, account_id, score, language, allowed)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (status_id)
        DO UPDATE SET account_id = $2, score = $3, language = $4, allowed = $5
        "#,
        status_id,
        account_id,
        score,
        language,
        allowed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// One surviving candidate for [`upsert_many`] — the columns [`upsert`] takes,
/// owned so a refresh pass can accumulate the whole set before flushing.
#[derive(Debug, Clone)]
pub struct TrendUpsert {
    pub status_id: i64,
    pub account_id: i64,
    pub score: f64,
    pub language: Option<String>,
    pub allowed: bool,
}

/// Batched [`upsert`] for one refresh pass: the whole surviving candidate set
/// in a single statement.
pub async fn upsert_many(pool: &PgPool, rows: &[TrendUpsert]) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    let status_ids: Vec<i64> = rows.iter().map(|r| r.status_id).collect();
    let account_ids: Vec<i64> = rows.iter().map(|r| r.account_id).collect();
    let scores: Vec<f64> = rows.iter().map(|r| r.score).collect();
    let languages: Vec<Option<String>> = rows.iter().map(|r| r.language.clone()).collect();
    let allowed: Vec<bool> = rows.iter().map(|r| r.allowed).collect();
    sqlx::query!(
        r#"
        INSERT INTO status_trends (status_id, account_id, score, language, allowed)
        SELECT * FROM unnest($1::bigint[], $2::bigint[], $3::float8[], $4::text[], $5::boolean[])
        ON CONFLICT (status_id)
        DO UPDATE SET account_id = EXCLUDED.account_id, score = EXCLUDED.score,
                      language = EXCLUDED.language, allowed = EXCLUDED.allowed
        "#,
        &status_ids,
        &account_ids,
        &scores,
        &languages as &[Option<String>],
        &allowed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Drops a status's trend row (its decayed score fell below the threshold).
pub async fn delete(pool: &PgPool, status_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM status_trends WHERE status_id = $1", status_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Batched [`delete`]: drops every listed status's trend row in one statement.
pub async fn delete_many(pool: &PgPool, status_ids: &[i64]) -> Result<(), DbError> {
    if status_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM status_trends WHERE status_id = ANY($1)",
        status_ids,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Recomputes every trend's 1-based `rank` by descending score within its
/// language bucket (Mastodon's `recalculate_ordered_rank`).
pub async fn recalculate_ranks(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE status_trends SET rank = ordered.calculated
        FROM (
            SELECT status_id,
                   row_number() OVER (PARTITION BY language ORDER BY score DESC) AS calculated
            FROM status_trends
        ) AS ordered
        WHERE status_trends.status_id = ordered.status_id
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Allowed (publicly surfaceable) trending statuses, highest score first, minus
/// authors hidden from the optional viewer — the public
/// `GET /api/v1/trends/statuses`.
pub async fn allowed(
    pool: &PgPool,
    viewer: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Status>, DbError> {
    let rows = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url,
               s.quote_approval_policy, s.application_id,
               s.title, s.object_type, s.external_url
        FROM status_trends tr
        JOIN statuses s ON s.id = tr.status_id
        JOIN accounts a ON a.id = s.account_id
        WHERE tr.allowed
          AND s.deleted_at IS NULL -- STUBFILTER
          AND NOT account_hidden($1, s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
        ORDER BY tr.score DESC, s.id DESC
        LIMIT $2 OFFSET $3
        "#,
        viewer,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// All trending statuses that are allowed or still pending, highest score first
/// — the moderator review view. A rejected status (`trendable = false`) is
/// dropped: rejecting it is a dismissal, so it leaves the review queue rather
/// than lingering with no visible effect.
pub async fn all_admin(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<Status>, DbError> {
    let rows = sqlx::query_as!(
        Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url,
               s.quote_approval_policy, s.application_id,
               s.title, s.object_type, s.external_url
        FROM status_trends tr
        JOIN statuses s ON s.id = tr.status_id
        WHERE s.trendable IS DISTINCT FROM false
          AND s.deleted_at IS NULL -- STUBFILTER
        ORDER BY tr.score DESC, s.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Of `status_ids`, those whose `trendable` is unset — i.e. that still
/// `requires_review?` (Mastodon: `trendable.nil? && account.requires_review?`;
/// Plamenu has no per-account review, so this reduces to the status override
/// being unset).
pub async fn unreviewed_ids(pool: &PgPool, status_ids: &[i64]) -> Result<HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM statuses -- STUBKEEP: projection over ids the caller already stub-filtered
           WHERE id = ANY($1) AND trendable IS NULL"#,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Sets a status's `trendable` override (approve/reject); the public listing
/// reflects it after the next refresh. Returns whether the status exists.
pub async fn set_trendable(
    pool: &PgPool,
    status_id: i64,
    trendable: bool,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE statuses SET trendable = $2 WHERE id = $1",
        status_id,
        trendable,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status::{self, NewLocalStatus};

    async fn author(pool: &PgPool) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: "author",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn post(pool: &PgPool, account_id: i64, body: &str) -> i64 {
        status::create_local(pool, NewLocalStatus::new(account_id, body, "public", None))
            .await
            .unwrap()
            .id
    }

    /// Only statuses with no `trendable` override still need review; an
    /// approved (`true`) or rejected (`false`) one is settled.
    #[sqlx::test]
    async fn unreviewed_ids_returns_only_statuses_without_an_override(pool: PgPool) {
        let author = author(&pool).await;
        let pending = post(&pool, author, "<p>pending</p>").await;
        let approved = post(&pool, author, "<p>approved</p>").await;
        let rejected = post(&pool, author, "<p>rejected</p>").await;
        set_trendable(&pool, approved, true).await.unwrap();
        set_trendable(&pool, rejected, false).await.unwrap();

        let unreviewed = unreviewed_ids(&pool, &[pending, approved, rejected])
            .await
            .unwrap();
        assert_eq!(unreviewed, HashSet::from([pending]));
        // Ids without a status row are simply absent, and an empty request is a
        // no-op rather than an error.
        assert!(unreviewed_ids(&pool, &[]).await.unwrap().is_empty());
    }

    /// The review queue drops a rejected status (dismissal) but keeps pending
    /// and approved ones, highest score first.
    #[sqlx::test]
    async fn all_admin_drops_rejected_and_orders_by_score(pool: PgPool) {
        let author = author(&pool).await;
        let pending = post(&pool, author, "<p>pending</p>").await;
        let approved = post(&pool, author, "<p>approved</p>").await;
        let rejected = post(&pool, author, "<p>rejected</p>").await;
        upsert(&pool, pending, author, 5.0, Some("en"), true)
            .await
            .unwrap();
        upsert(&pool, approved, author, 20.0, Some("en"), true)
            .await
            .unwrap();
        upsert(&pool, rejected, author, 10.0, Some("en"), true)
            .await
            .unwrap();
        set_trendable(&pool, approved, true).await.unwrap();
        set_trendable(&pool, rejected, false).await.unwrap();

        let ids: Vec<i64> = all_admin(&pool, 20, 0)
            .await
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, [approved, pending], "rejected dropped, score-ordered");
    }
}
