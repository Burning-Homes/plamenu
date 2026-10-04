//! Trending-tag ranking state — Mastodon's `tag_trends` and the persistence the
//! `Trends::Tags` scoring engine drives. The score math itself lives in the
//! server crate (`crate::trends`); this module is the SQL behind it plus the
//! read queries for the public and admin trends endpoints.

use std::collections::HashMap;

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::DbError;
use crate::tag::{AdminTag, DayCount, Tag, ViewerFlags};

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

/// A ranked tag page and its visible history/viewer state from one SQL snapshot.
#[derive(Default)]
pub struct AllowedTagPage {
    pub tags: Vec<Tag>,
    pub flags: ViewerFlags,
    pub histories: HashMap<i64, Vec<DayCount>>,
}

/// Pages tags before joining sidecars, so history rows cannot consume the page
/// limit. The caller bounds the page to 20 tags and the history to seven days.
/// Both viewer reasons are aggregated independently; anonymous viewers have
/// no flag rows. Each page tag aggregates its indexed daily counts before the
/// final join, avoiding a sort of the whole page's raw usage rows. All sidecars
/// still come from one statement snapshot.
#[allow(
    clippy::too_many_lines,
    reason = "one typed page query and its row assembly"
)]
pub async fn allowed_page(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    viewer_account_id: Option<i64>,
    from: Date,
    today: Date,
) -> Result<AllowedTagPage, DbError> {
    let rows = sqlx::query!(
        r#"
        WITH page AS MATERIALIZED (
            SELECT t.id, t.name, t.display_name, tr.score
            FROM tag_trends tr JOIN tags t ON t.id = tr.tag_id
            WHERE tr.allowed
            ORDER BY tr.score DESC, t.id DESC
            LIMIT $1 OFFSET $2
        ), flags AS (
            SELECT tag_id, bool_or(following) AS following,
                   bool_or(NOT following) AS featuring
            FROM (
                SELECT f.tag_id, true AS following
                FROM tag_follows f JOIN page p ON p.id = f.tag_id
                WHERE f.account_id = $3
                UNION ALL
                SELECT f.tag_id, false AS following
                FROM featured_tags f JOIN page p ON p.id = f.tag_id
                WHERE f.account_id = $3
            ) memberships
            GROUP BY tag_id
        )
        SELECT p.id AS "id!", p.name AS "name!", p.display_name,
               h.day AS "day?", h.uses AS "uses?", h.accounts AS "accounts?",
               f.following AS "following?", f.featuring AS "featuring?"
        FROM page p
        LEFT JOIN LATERAL (
            SELECT u.day, COALESCE(sum(u.uses), 0)::bigint AS uses,
                   count(*)::bigint AS accounts
            FROM tag_usages u
            WHERE u.tag_id = p.id AND u.day BETWEEN $4 AND $5
            GROUP BY u.day
        ) h ON true
        LEFT JOIN flags f ON f.tag_id = p.id
        ORDER BY p.score DESC, p.id DESC, h.day DESC
        "#,
        limit,
        offset,
        viewer_account_id,
        from,
        today,
    )
    .fetch_all(pool)
    .await?;
    let mut page = AllowedTagPage::default();
    for row in rows {
        if page.tags.last().is_none_or(|tag| tag.id != row.id) {
            page.tags.push(Tag {
                id: row.id,
                name: row.name,
                display_name: row.display_name,
            });
            if row.following == Some(true) {
                page.flags.following.insert(row.id);
            }
            if row.featuring == Some(true) {
                page.flags.featuring.insert(row.id);
            }
        }
        if let Some(day) = row.day {
            page.histories.entry(row.id).or_default().push(DayCount {
                tag_id: row.id,
                day,
                uses: row.uses.unwrap_or_default(),
                accounts: row.accounts.unwrap_or_default(),
            });
        }
    }
    Ok(page)
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
