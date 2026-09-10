//! Trending-tag ranking state — Mastodon's `tag_trends` and the persistence the
//! `Trends::Tags` scoring engine drives. The score math itself lives in the
//! server crate (`crate::trends`); this module is the SQL behind it plus the
//! read queries for the public and admin trends endpoints.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::DbError;
use crate::tag::{AdminTag, Tag};

/// A tag in scope for a refresh pass — it either already has a trend row or was
/// used today. Carries the two distinct-account counts the score compares and
/// the tag's peak-score bookkeeping.
#[derive(Debug, Clone)]
pub struct ScoreInput {
    pub tag_id: i64,
    pub trendable: Option<bool>,
    /// Distinct accounts that used the tag today.
    pub observed: i64,
    /// Distinct accounts that used it yesterday (raw; 0 when none).
    pub expected: i64,
    pub max_score: Option<f64>,
    pub max_score_at: Option<OffsetDateTime>,
}

/// Every tag to (re)score this pass: the union of tags with a live trend row and
/// tags used today, with today's/yesterday's account counts attached.
pub async fn score_inputs(
    pool: &PgPool,
    today: Date,
    yesterday: Date,
) -> Result<Vec<ScoreInput>, DbError> {
    let rows = sqlx::query_as!(
        ScoreInput,
        r#"
        SELECT t.id AS "tag_id!", t.trendable,
               (SELECT count(*) FROM tag_usages u WHERE u.tag_id = t.id AND u.day = $1)
                   AS "observed!",
               (SELECT count(*) FROM tag_usages u WHERE u.tag_id = t.id AND u.day = $2)
                   AS "expected!",
               t.max_score, t.max_score_at
        FROM tags t
        WHERE t.id IN (
            SELECT tag_id FROM tag_trends
            UNION
            SELECT tag_id FROM tag_usages WHERE day = $1
        )
        "#,
        today,
        yesterday,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Records a new peak score and when it was reached (Mastodon's
/// `update_columns(max_score:, max_score_at:)`).
pub async fn set_peak(
    pool: &PgPool,
    tag_id: i64,
    max_score: f64,
    at: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE tags SET max_score = $2, max_score_at = $3 WHERE id = $1",
        tag_id,
        max_score,
        at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`set_peak`] for one refresh pass: every `(tag_id, max_score)` pair
/// is stamped with the same instant in a single statement.
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
        UPDATE tags SET max_score = v.max_score, max_score_at = $3
        FROM unnest($1::bigint[], $2::float8[]) AS v(id, max_score)
        WHERE tags.id = v.id
        "#,
        &ids,
        &scores,
        at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts or updates a tag's trend row with its decayed score and current
/// allowed flag.
pub async fn upsert(pool: &PgPool, tag_id: i64, score: f64, allowed: bool) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO tag_trends (tag_id, score, allowed)
        VALUES ($1, $2, $3)
        ON CONFLICT (tag_id) DO UPDATE SET score = $2, allowed = $3
        "#,
        tag_id,
        score,
        allowed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`upsert`] for one refresh pass: the whole surviving candidate set
/// in a single statement.
pub async fn upsert_many(pool: &PgPool, rows: &[(i64, f64, bool)]) -> Result<(), DbError> {
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
    let scores: Vec<f64> = rows.iter().map(|(_, score, _)| *score).collect();
    let allowed: Vec<bool> = rows.iter().map(|(.., allowed)| *allowed).collect();
    sqlx::query!(
        r#"
        INSERT INTO tag_trends (tag_id, score, allowed)
        SELECT * FROM unnest($1::bigint[], $2::float8[], $3::boolean[])
        ON CONFLICT (tag_id) DO UPDATE SET score = EXCLUDED.score, allowed = EXCLUDED.allowed
        "#,
        &ids,
        &scores,
        &allowed,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Drops a tag's trend row (its decayed score fell below the threshold).
pub async fn delete(pool: &PgPool, tag_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM tag_trends WHERE tag_id = $1", tag_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Batched [`delete`]: drops every listed tag's trend row in one statement.
pub async fn delete_many(pool: &PgPool, tag_ids: &[i64]) -> Result<(), DbError> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!("DELETE FROM tag_trends WHERE tag_id = ANY($1)", tag_ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Recomputes every trend's 1-based `rank` by descending score within its
/// language bucket (Mastodon's `recalculate_ordered_rank`).
pub async fn recalculate_ranks(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        UPDATE tag_trends SET rank = ordered.calculated
        FROM (
            SELECT tag_id,
                   row_number() OVER (PARTITION BY language ORDER BY score DESC) AS calculated
            FROM tag_trends
        ) AS ordered
        WHERE tag_trends.tag_id = ordered.tag_id
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Allowed (publicly surfaceable) trending tags, highest score first — the
/// public `GET /api/v1/trends/tags` listing.
pub async fn allowed(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<Tag>, DbError> {
    let rows = sqlx::query_as!(
        Tag,
        r#"
        SELECT t.id, t.name, t.display_name
        FROM tag_trends tr
        JOIN tags t ON t.id = tr.tag_id
        WHERE tr.allowed
        ORDER BY tr.score DESC, t.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// All trending tags (allowed or not), highest score first — the moderator view
/// of `GET /api/v1/admin/trends/tags`. Returns admin tag rows for the
/// `Admin::Tag` serializer.
pub async fn all_admin(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<AdminTag>, DbError> {
    let rows = sqlx::query_as!(
        AdminTag,
        r#"
        SELECT t.id, t.name, t.display_name, t.usable, t.listable, t.trendable,
               t.reviewed_at
        FROM tag_trends tr
        JOIN tags t ON t.id = tr.tag_id
        ORDER BY tr.score DESC, t.id DESC
        LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
