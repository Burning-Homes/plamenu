//! Trending-link ranking state — Mastodon's `preview_card_trends` plus the
//! `Trends::Links` usage history. Links score like tags (distinct accounts today
//! vs yesterday, peak-decayed), so this module carries a `preview_card_usages`
//! rollup written when an appropriate card is attached to an eligible status.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::DbError;
use crate::preview_card::PreviewCard;

/// Records a card's usage when it is attached to `status_id`, mirroring
/// Mastodon's `Trends::Links.register` + `appropriate_for_trends?`: skipped for
/// reblogs, non-public/sensitive/CW statuses, silenced authors, and cards that
/// aren't article-like link cards (title + description + image + provider).
pub async fn record_use(
    pool: &PgPool,
    preview_card_id: i64,
    status_id: i64,
    day: Date,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO preview_card_usages (preview_card_id, day, account_id, uses)
        SELECT $1, $3, s.account_id, 1
        FROM statuses s
        JOIN accounts a ON a.id = s.account_id
        JOIN preview_cards c ON c.id = $1
        WHERE s.id = $2
          AND s.reblog_of_id IS NULL
          AND s.visibility = 'public'
          AND NOT account_silenced(a.id)
          AND s.sensitive = false
          AND (s.spoiler_text IS NULL OR s.spoiler_text = '')
          AND c.kind = 'link'
          AND c.title <> '' AND c.description <> ''
          AND c.image_url IS NOT NULL AND c.provider_name <> ''
        ON CONFLICT (preview_card_id, day, account_id)
        DO UPDATE SET uses = preview_card_usages.uses + 1
        "#,
        preview_card_id,
        status_id,
        day,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A card in scope for a refresh pass — it has a live trend row or was used
/// today — with its account counts and peak bookkeeping.
#[derive(Debug, Clone)]
pub struct ScoreInput {
    pub preview_card_id: i64,
    pub url: String,
    pub language: Option<String>,
    pub trendable: Option<bool>,
    pub observed: i64,
    pub expected: i64,
    pub max_score: Option<f64>,
    pub max_score_at: Option<OffsetDateTime>,
}

/// Every card to (re)score this pass, with today's/yesterday's distinct-account
/// counts attached.
pub async fn score_inputs(
    pool: &PgPool,
    today: Date,
    yesterday: Date,
) -> Result<Vec<ScoreInput>, DbError> {
    let rows = sqlx::query_as!(
        ScoreInput,
        r#"
        SELECT c.id AS "preview_card_id!", c.url AS "url!", c.language, c.trendable,
               (SELECT count(*) FROM preview_card_usages u
                    WHERE u.preview_card_id = c.id AND u.day = $1) AS "observed!",
               (SELECT count(*) FROM preview_card_usages u
                    WHERE u.preview_card_id = c.id AND u.day = $2) AS "expected!",
               c.max_score, c.max_score_at
        FROM preview_cards c
        WHERE c.id IN (
            SELECT preview_card_id FROM preview_card_trends
            UNION
            SELECT preview_card_id FROM preview_card_usages WHERE day = $1
        )
        "#,
        today,
        yesterday,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Records a new peak score and when it was reached.
pub async fn set_peak(
    pool: &PgPool,
    preview_card_id: i64,
    max_score: f64,
    at: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE preview_cards SET max_score = $2, max_score_at = $3 WHERE id = $1",
        preview_card_id,
        max_score,
        at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`set_peak`] for one refresh pass: every `(preview_card_id,
/// max_score)` pair is stamped with the same instant in a single statement.
pub async fn set_peak_many(
    pool: &PgPool,
    peaks: &[(i64, f64)],
    at: OffsetDateTime,
) -> Result<(), DbError> {
    if peaks.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = peaks.iter().map(|(id, _)| *id).collect();
    let scores: Vec<f64> = peaks.iter().map(|(_, score)| *score).collect();
    sqlx::query!(
        r#"
        UPDATE preview_cards SET max_score = v.max_score, max_score_at = $3
        FROM unnest($1::bigint[], $2::float8[]) AS v(id, max_score)
        WHERE preview_cards.id = v.id
        "#,
        &ids,
        &scores,
        at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts or updates a card's trend row with its decayed score.
pub async fn upsert(
    pool: &PgPool,
    preview_card_id: i64,
    score: f64,
    language: Option<&str>,
    allowed: bool,
) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO preview_card_trends (preview_card_id, score, language, allowed)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (preview_card_id)
        DO UPDATE SET score = $2, language = $3, allowed = $4
        "#,
        preview_card_id,
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
    pub preview_card_id: i64,
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
    let card_ids: Vec<i64> = rows.iter().map(|r| r.preview_card_id).collect();
    let scores: Vec<f64> = rows.iter().map(|r| r.score).collect();
    let languages: Vec<Option<String>> = rows.iter().map(|r| r.language.clone()).collect();
    let allowed: Vec<bool> = rows.iter().map(|r| r.allowed).collect();
    sqlx::query!(
        r#"
        INSERT INTO preview_card_trends (preview_card_id, score, language, allowed)
        SELECT * FROM unnest($1::bigint[], $2::float8[], $3::text[], $4::boolean[])
        ON CONFLICT (preview_card_id)
        DO UPDATE SET score = EXCLUDED.score, language = EXCLUDED.language,
                      allowed = EXCLUDED.allowed
        "#,
        &card_ids,
        &scores,
        &languages as &[Option<String>],
        &allowed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Drops a card's trend row.
pub async fn delete(pool: &PgPool, preview_card_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM preview_card_trends WHERE preview_card_id = $1",
        preview_card_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`delete`]: drops every listed card's trend row in one statement.
pub async fn delete_many(pool: &PgPool, preview_card_ids: &[i64]) -> Result<(), DbError> {
    if preview_card_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "DELETE FROM preview_card_trends WHERE preview_card_id = ANY($1)",
        preview_card_ids,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Recomputes every trend's 1-based `rank` by descending score within language.
pub async fn recalculate_ranks(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE preview_card_trends SET rank = ordered.calculated
        FROM (
            SELECT preview_card_id,
                   row_number() OVER (PARTITION BY language ORDER BY score DESC) AS calculated
            FROM preview_card_trends
        ) AS ordered
        WHERE preview_card_trends.preview_card_id = ordered.preview_card_id
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Allowed trending links, highest score first — the public
/// `GET /api/v1/trends/links`.
pub async fn allowed(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<PreviewCard>, DbError> {
    let rows = sqlx::query_as!(
        PreviewCard,
        r#"
        SELECT c.id, c.url, c.title, c.description, c.kind, c.author_name,
               c.author_url, c.provider_name, c.provider_url, c.html, c.width,
               c.height, c.image_url, c.image_file_name, c.image_content_type,
               c.image_file_size, c.image_cached_at, c.image_description,
               c.embed_url, c.language, c.published_at, c.author_account_id,
               c.updated_at
        FROM preview_card_trends tr
        JOIN preview_cards c ON c.id = tr.preview_card_id
        WHERE tr.allowed
        ORDER BY tr.score DESC, c.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// All trending links that are allowed or still pending, highest score first —
/// the moderator review view. A rejected card (`trendable = false`) is dropped:
/// rejecting it is a dismissal, so it leaves the review queue rather than
/// lingering with no visible effect.
pub async fn all_admin(
    pool: &PgPool,
    limit: i64,
    offset: i64,
) -> Result<Vec<PreviewCard>, DbError> {
    let rows = sqlx::query_as!(
        PreviewCard,
        r#"
        SELECT c.id, c.url, c.title, c.description, c.kind, c.author_name,
               c.author_url, c.provider_name, c.provider_url, c.html, c.width,
               c.height, c.image_url, c.image_file_name, c.image_content_type,
               c.image_file_size, c.image_cached_at, c.image_description,
               c.embed_url, c.language, c.published_at, c.author_account_id,
               c.updated_at
        FROM preview_card_trends tr
        JOIN preview_cards c ON c.id = tr.preview_card_id
        WHERE c.trendable IS DISTINCT FROM false
        ORDER BY tr.score DESC, c.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Public statuses linking to an allowed-trending card at `url`, newest first,
/// keyset-paginated by status id, minus authors hidden from the viewer — the
/// `GET /api/v1/timelines/link` feed. Empty when the card isn't (allowed)
/// trending.
pub async fn link_timeline(
    pool: &PgPool,
    url: &str,
    viewer: Option<i64>,
    max_id: Option<i64>,
    limit: i64,
) -> Result<Vec<crate::status::Status>, DbError> {
    let rows = sqlx::query_as!(
        crate::status::Status,
        r#"
        SELECT s.id, s.uri, s.account_id, s.content, s.created_at, s.updated_at,
               s.visibility, s.in_reply_to_id, s.reblog_of_id, s.edited_at,
               s.spoiler_text, s.sensitive, s.language, s.url,
               s.quote_approval_policy, s.application_id,
               s.title, s.object_type, s.external_url
        FROM preview_card_trends tr
        JOIN preview_cards c ON c.id = tr.preview_card_id
        JOIN preview_cards_statuses pcs ON pcs.preview_card_id = c.id
        JOIN statuses s ON s.id = pcs.status_id
        JOIN accounts a ON a.id = s.account_id
        WHERE tr.allowed AND c.url = $1
          AND s.deleted_at IS NULL -- STUBFILTER
          AND s.visibility = 'public' AND s.reblog_of_id IS NULL
          AND NOT account_hidden($2, s.account_id)
          AND NOT account_silenced(s.account_id)
          AND (a.portable OR instance_domain_allowed(a.domain))
          AND ($3::bigint IS NULL OR s.id < $3)
        ORDER BY s.id DESC
        LIMIT $4
        "#,
        url,
        viewer,
        max_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Of `card_ids`, those whose `trendable` is unset — i.e. still pending review
/// (mirrors [`crate::status_trend::unreviewed_ids`]). The remaining listed cards
/// are allowed, since rejected ones are filtered out of the review view.
pub async fn unreviewed_ids(pool: &PgPool, card_ids: &[i64]) -> Result<HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT id AS "id!" FROM preview_cards
           WHERE id = ANY($1) AND trendable IS NULL"#,
        card_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// A card's own `trendable` override (for resolving `allowed` and
/// `requires_review`), or `None` if the card is unknown.
pub async fn trendable_of(pool: &PgPool, card_id: i64) -> Result<Option<Option<bool>>, DbError> {
    let row = sqlx::query_scalar!("SELECT trendable FROM preview_cards WHERE id = $1", card_id,)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Sets a card's `trendable` override (approve/reject). Returns whether it
/// exists.
pub async fn set_trendable(pool: &PgPool, card_id: i64, trendable: bool) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE preview_cards SET trendable = $2 WHERE id = $1",
        card_id,
        trendable,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// One day of a card's usage.
#[derive(Debug, Clone)]
pub struct DayCount {
    pub preview_card_id: i64,
    pub day: Date,
    pub uses: i64,
    pub accounts: i64,
}

/// Per-day usage for a batch of cards on or after `from`, keyed by card id —
/// feeds the `history` array of the `Trends::Link` entity.
pub async fn history_batch(
    pool: &PgPool,
    card_ids: &[i64],
    from: Date,
) -> Result<HashMap<i64, Vec<DayCount>>, DbError> {
    let rows = sqlx::query_as!(
        DayCount,
        r#"
        SELECT preview_card_id AS "preview_card_id!", day AS "day!",
               COALESCE(sum(uses), 0)::bigint AS "uses!",
               count(*)::bigint AS "accounts!"
        FROM preview_card_usages
        WHERE preview_card_id = ANY($1) AND day >= $2
        GROUP BY preview_card_id, day
        "#,
        card_ids,
        from,
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, Vec<DayCount>> = HashMap::new();
    for row in rows {
        map.entry(row.preview_card_id).or_default().push(row);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview_card::{self, NewPreviewCard};

    async fn card(pool: &PgPool, url: &str, title: &str) -> i64 {
        preview_card::upsert(
            pool,
            NewPreviewCard {
                url,
                title,
                kind: "link",
                provider_name: "Example",
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Mirrors [`crate::status_trend::unreviewed_ids`]: only cards without a
    /// `trendable` override still require review.
    #[sqlx::test]
    async fn unreviewed_ids_returns_only_cards_without_an_override(pool: PgPool) {
        let pending = card(&pool, "https://ex.test/pending", "Pending").await;
        let approved = card(&pool, "https://ex.test/approved", "Approved").await;
        let rejected = card(&pool, "https://ex.test/rejected", "Rejected").await;
        set_trendable(&pool, approved, true).await.unwrap();
        set_trendable(&pool, rejected, false).await.unwrap();

        let unreviewed = unreviewed_ids(&pool, &[pending, approved, rejected])
            .await
            .unwrap();
        assert_eq!(unreviewed, HashSet::from([pending]));
        assert!(unreviewed_ids(&pool, &[]).await.unwrap().is_empty());
    }

    /// The review queue drops a rejected card (dismissal) but keeps pending and
    /// approved ones, highest score first.
    #[sqlx::test]
    async fn all_admin_drops_rejected_and_orders_by_score(pool: PgPool) {
        let pending = card(&pool, "https://ex.test/pending", "Pending").await;
        let approved = card(&pool, "https://ex.test/approved", "Approved").await;
        let rejected = card(&pool, "https://ex.test/rejected", "Rejected").await;
        upsert(&pool, pending, 5.0, Some("en"), true).await.unwrap();
        upsert(&pool, approved, 20.0, Some("en"), true)
            .await
            .unwrap();
        upsert(&pool, rejected, 10.0, Some("en"), true)
            .await
            .unwrap();
        set_trendable(&pool, approved, true).await.unwrap();
        set_trendable(&pool, rejected, false).await.unwrap();

        let ids: Vec<i64> = all_admin(&pool, 20, 0)
            .await
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, [approved, pending], "rejected dropped, score-ordered");
    }
}
