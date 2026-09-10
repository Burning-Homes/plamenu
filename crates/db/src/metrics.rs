//! Admin dashboard metrics — the SQL behind `POST /api/v1/admin/measures`,
//! `…/dimensions` and `…/retention` (Mastodon's `Admin::Metrics::*`).
//!
//! Each measure mirrors a Mastodon `Admin::Metrics::Measure::*` class: a `total`
//! (and, for in-range measures, a `previous_total`) plus a per-day `data` series
//! built off a `generate_series(...)` day axis with a correlated per-day count.
//! All day bucketing is pinned to UTC (`… AT TIME ZONE 'UTC'`) so results are
//! independent of the Postgres session `TimeZone`, matching Mastodon's UTC
//! reckoning. Dimensions return a flat key→value ranking.
//!
//! **This stays UTC deliberately, and is the one place the web UI does not
//! render in the viewer's zone.** Bucketing per admin would make two
//! moderators in different zones see different numbers for the same instance
//! and would diverge from what `/api/v1/admin/measures` reports to clients. The
//! dashboard states "Daily figures are counted in UTC days" instead. A *user
//! quota* faces the opposite way and does follow the owner's zone — see
//! [`crate::scheduled_status::count_on_day`].
//!
//! Note: a measure's `total` counts the *full* `[first_day, last_day]` window
//! (half-open `>= first_day AND < last_day + 1`), so it equals the sum of the
//! daily `data` series. Mastodon bounds its total with a `BETWEEN date AND date`
//! that silently drops most of the final day, leaving `total` < `sum(data)`; we
//! fix that inconsistency rather than reproduce it.
//!
//! The two tag-history measures (`tag_accounts`, `tag_uses`) read the
//! `tag_usages` daily rollup (Postgres stand-in for Mastodon's Redis
//! trend history) rather than a live scan of `status_tags`.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::DbError;

/// One point on a measure's daily series.
#[derive(Debug, Clone)]
pub struct SeriesPoint {
    pub period: Date,
    pub value: i64,
}

/// A measure's three serialized fields. `previous_total` is `None` for the
/// `instance_*` measures (Mastodon's `total_in_time_range? == false`).
#[derive(Debug, Clone)]
pub struct Measurement {
    pub total: i64,
    pub previous_total: Option<i64>,
    pub data: Vec<SeriesPoint>,
}

/// One row of a dimension ranking. `key` is `None` where the grouping column is
/// `NULL` (e.g. a local-account `accounts.domain`), which the serializer renders
/// as the local domain / a default label.
#[derive(Debug, Clone)]
pub struct DimRow {
    pub key: Option<String>,
    pub value: i64,
}

/// The previous comparison window: the same length immediately before `[start,
/// end]`. Mirrors Mastodon's `previous_time_period`.
fn previous_window(start: OffsetDateTime, end: OffsetDateTime) -> (OffsetDateTime, OffsetDateTime) {
    let len = end - start;
    (start - len, end - len)
}

// ---------------------------------------------------------------------------
// Measures — no schema dependencies
// ---------------------------------------------------------------------------

/// `new_users` — local registrations per day.
pub async fn new_users(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Measurement, DbError> {
    let total = new_users_total(pool, start, end).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = new_users_total(pool, ps, pe).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM users
            WHERE date_trunc('day', users.created_at AT TIME ZONE 'UTC')::date = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn new_users_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM users
           WHERE users.created_at >= ($1 AT TIME ZONE 'UTC')::date
             AND users.created_at < (($2 AT TIME ZONE 'UTC')::date + 1)"#,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `opened_reports` — reports filed per day.
pub async fn opened_reports(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Measurement, DbError> {
    let total = opened_reports_total(pool, start, end).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = opened_reports_total(pool, ps, pe).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM reports
            WHERE date_trunc('day', reports.created_at AT TIME ZONE 'UTC')::date = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn opened_reports_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM reports
           WHERE reports.created_at >= ($1 AT TIME ZONE 'UTC')::date
             AND reports.created_at < (($2 AT TIME ZONE 'UTC')::date + 1)"#,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `resolved_reports` — reports resolved per day (by `action_taken_at`).
pub async fn resolved_reports(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Measurement, DbError> {
    let total = resolved_reports_total(pool, start, end).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = resolved_reports_total(pool, ps, pe).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM reports
            WHERE reports.action_taken_at IS NOT NULL
              AND date_trunc('day', reports.action_taken_at AT TIME ZONE 'UTC')::date = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn resolved_reports_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM reports
           WHERE reports.action_taken_at IS NOT NULL
             AND reports.action_taken_at >= ($1 AT TIME ZONE 'UTC')::date
             AND reports.action_taken_at < (($2 AT TIME ZONE 'UTC')::date + 1)"#,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

// --- instance_* measures: per-domain, `total` is an all-time count (no range),
//     `previous_total` is None (Mastodon `total_in_time_range? == false`). ---

/// `instance_accounts` — accounts known from `domain` (optionally its
/// subdomains); total is the all-time count, series is per-day new accounts.
pub async fn instance_accounts(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM accounts
           WHERE accounts.domain = $1
              OR ($2 AND accounts.domain LIKE '%.' || $1)"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM accounts
            WHERE date_trunc('day', accounts.created_at AT TIME ZONE 'UTC')::date = axis.period
              AND (accounts.domain = $3 OR ($4 AND accounts.domain LIKE '%.' || $3))
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// `instance_statuses` — statuses authored by accounts from `domain`. Uses the
/// time-ordered `statuses.id` range as a coarse pre-filter (Mastodon snowflake
/// trick) plus the per-day `created_at` bucket.
pub async fn instance_statuses(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    earliest_id: i64,
    latest_id: i64,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!"
           FROM statuses
           INNER JOIN accounts ON accounts.id = statuses.account_id
           WHERE statuses.deleted_at IS NULL -- STUBFILTER
             AND (accounts.domain = $1 OR ($2 AND accounts.domain LIKE '%.' || $1))"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*)
            FROM statuses
            INNER JOIN accounts ON accounts.id = statuses.account_id
            WHERE statuses.id BETWEEN $3 AND $4
              AND statuses.deleted_at IS NULL -- STUBFILTER
              AND (accounts.domain = $5 OR ($6 AND accounts.domain LIKE '%.' || $5))
              AND date_trunc('day', statuses.created_at AT TIME ZONE 'UTC')::date = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        earliest_id,
        latest_id,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// `instance_follows` — follows *targeting* accounts from `domain`.
pub async fn instance_follows(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!"
           FROM follows
           INNER JOIN accounts ON accounts.id = follows.target_account_id
           WHERE accounts.domain = $1 OR ($2 AND accounts.domain LIKE '%.' || $1)"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*)
            FROM follows
            INNER JOIN accounts ON accounts.id = follows.target_account_id
            WHERE date_trunc('day', follows.created_at AT TIME ZONE 'UTC')::date = axis.period
              AND (accounts.domain = $3 OR ($4 AND accounts.domain LIKE '%.' || $3))
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// `instance_followers` — follows *originating from* accounts of `domain`.
pub async fn instance_followers(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!"
           FROM follows
           INNER JOIN accounts ON accounts.id = follows.account_id
           WHERE accounts.domain = $1 OR ($2 AND accounts.domain LIKE '%.' || $1)"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*)
            FROM follows
            INNER JOIN accounts ON accounts.id = follows.account_id
            WHERE date_trunc('day', follows.created_at AT TIME ZONE 'UTC')::date = axis.period
              AND (accounts.domain = $3 OR ($4 AND accounts.domain LIKE '%.' || $3))
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// `instance_reports` — reports filed against accounts from `domain`.
pub async fn instance_reports(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!"
           FROM reports
           INNER JOIN accounts ON accounts.id = reports.target_account_id
           WHERE accounts.domain = $1 OR ($2 AND accounts.domain LIKE '%.' || $1)"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*)
            FROM reports
            INNER JOIN accounts ON accounts.id = reports.target_account_id
            WHERE date_trunc('day', reports.created_at AT TIME ZONE 'UTC')::date = axis.period
              AND (accounts.domain = $3 OR ($4 AND accounts.domain LIKE '%.' || $3))
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// `tag_servers` measure — distinct domains using a tag per day. `total`/
/// `previous_total` are the distinct-domain counts over the (snowflake-bounded)
/// window and the preceding one.
pub async fn tag_servers_measure(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    window: (i64, i64),
    prev_window: (i64, i64),
    tag_id: i64,
) -> Result<Measurement, DbError> {
    let (earliest_id, latest_id) = window;
    let total = tag_servers_distinct(pool, earliest_id, latest_id, tag_id).await?;
    let previous_total = tag_servers_distinct(pool, prev_window.0, prev_window.1, tag_id).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM (
                SELECT DISTINCT accounts.domain
                FROM statuses -- STUBKEEP: usage aggregate
                INNER JOIN status_tags ON statuses.id = status_tags.status_id
                INNER JOIN accounts ON statuses.account_id = accounts.id
                WHERE status_tags.tag_id = $3
                  AND statuses.id BETWEEN $4 AND $5
                  AND date_trunc('day', statuses.created_at AT TIME ZONE 'UTC')::date = axis.period
            ) AS d
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        tag_id,
        earliest_id,
        latest_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn tag_servers_distinct(
    pool: &PgPool,
    earliest_id: i64,
    latest_id: i64,
    tag_id: i64,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT accounts.domain) AS "c!"
           FROM statuses -- STUBKEEP: usage aggregate
           INNER JOIN status_tags ON statuses.id = status_tags.status_id
           INNER JOIN accounts ON statuses.account_id = accounts.id
           WHERE status_tags.tag_id = $1 AND statuses.id BETWEEN $2 AND $3"#,
        tag_id,
        earliest_id,
        latest_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `tag_uses` measure — a tag's total usage per day, off the `tag_usages`
/// rollup (Mastodon's `Trends::History` uses). `total`/`previous_total` sum the
/// window's daily uses; unlike `tag_accounts` these are additive across days.
pub async fn tag_uses_measure(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    tag_id: i64,
) -> Result<Measurement, DbError> {
    let total = tag_uses_total(pool, start, end, tag_id).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = tag_uses_total(pool, ps, pe, tag_id).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT COALESCE(sum(uses), 0)::bigint FROM tag_usages
            WHERE tag_id = $3 AND day = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        tag_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn tag_uses_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    tag_id: i64,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(uses), 0)::bigint AS "c!" FROM tag_usages
           WHERE tag_id = $1
             AND day >= ($2 AT TIME ZONE 'UTC')::date
             AND day <= ($3 AT TIME ZONE 'UTC')::date"#,
        tag_id,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `tag_accounts` measure — a tag's distinct accounts per day. The `total`
/// counts accounts distinct across the *whole* window (Mastodon's `HyperLogLog`
/// union), which is why it is not the sum of the daily series.
pub async fn tag_accounts_measure(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    tag_id: i64,
) -> Result<Measurement, DbError> {
    let total = tag_accounts_distinct(pool, start, end, tag_id).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = tag_accounts_distinct(pool, ps, pe, tag_id).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(*) FROM tag_usages
            WHERE tag_id = $3 AND day = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        tag_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn tag_accounts_distinct(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    tag_id: i64,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT account_id) AS "c!" FROM tag_usages
           WHERE tag_id = $1
             AND day >= ($2 AT TIME ZONE 'UTC')::date
             AND day <= ($3 AT TIME ZONE 'UTC')::date"#,
        tag_id,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

// ---------------------------------------------------------------------------
// Dimensions — no schema dependencies
// ---------------------------------------------------------------------------

/// `servers` — top domains by status volume in the window.
pub async fn dim_servers(
    pool: &PgPool,
    earliest_id: i64,
    latest_id: i64,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT accounts.domain AS "key", count(*) AS "value!"
        FROM statuses -- STUBKEEP: usage aggregate
        INNER JOIN accounts ON accounts.id = statuses.account_id
        WHERE statuses.id BETWEEN $1 AND $2
          AND statuses.deleted_at IS NULL -- STUBFILTER
        GROUP BY accounts.domain
        ORDER BY count(*) DESC
        LIMIT $3
        "#,
        earliest_id,
        latest_id,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// `instance_accounts` dimension — most-followed accounts on `domain`.
pub async fn dim_instance_accounts(
    pool: &PgPool,
    domain: &str,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT accounts.username AS "key", count(follows.*) AS "value!"
        FROM accounts
        LEFT JOIN follows ON follows.target_account_id = accounts.id
        WHERE accounts.domain = $1
        GROUP BY accounts.id, accounts.username
        ORDER BY count(follows.*) DESC
        LIMIT $2
        "#,
        domain,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// `instance_languages` dimension — language breakdown of `domain`'s statuses.
pub async fn dim_instance_languages(
    pool: &PgPool,
    domain: &str,
    earliest_id: i64,
    latest_id: i64,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT COALESCE(statuses.language, 'und') AS "key!", count(*) AS "value!"
        FROM statuses -- STUBKEEP: usage aggregate
        INNER JOIN accounts ON accounts.id = statuses.account_id
        WHERE accounts.domain = $1
          AND statuses.id BETWEEN $2 AND $3
          AND statuses.reblog_of_id IS NULL
          AND statuses.deleted_at IS NULL -- STUBFILTER
        GROUP BY COALESCE(statuses.language, 'und')
        ORDER BY count(*) DESC
        LIMIT $4
        "#,
        domain,
        earliest_id,
        latest_id,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// `tag_servers` dimension — top domains using a tag.
pub async fn dim_tag_servers(
    pool: &PgPool,
    tag_id: i64,
    earliest_id: i64,
    latest_id: i64,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT accounts.domain AS "key", count(*) AS "value!"
        FROM statuses -- STUBKEEP: usage aggregate
        INNER JOIN accounts ON accounts.id = statuses.account_id
        INNER JOIN status_tags ON status_tags.status_id = statuses.id
        WHERE status_tags.tag_id = $1
          AND statuses.id BETWEEN $2 AND $3
        GROUP BY accounts.domain
        ORDER BY count(*) DESC
        LIMIT $4
        "#,
        tag_id,
        earliest_id,
        latest_id,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// `tag_languages` dimension — language breakdown of a tag's statuses.
pub async fn dim_tag_languages(
    pool: &PgPool,
    tag_id: i64,
    earliest_id: i64,
    latest_id: i64,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT COALESCE(statuses.language, 'und') AS "key!", count(*) AS "value!"
        FROM statuses -- STUBKEEP: usage aggregate
        INNER JOIN status_tags ON status_tags.status_id = statuses.id
        WHERE status_tags.tag_id = $1
          AND statuses.id BETWEEN $2 AND $3
        GROUP BY COALESCE(statuses.language, 'und')
        ORDER BY count(*) DESC
        LIMIT $4
        "#,
        tag_id,
        earliest_id,
        latest_id,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

// ---------------------------------------------------------------------------
// Sign-in-backed measures & dimensions
// ---------------------------------------------------------------------------

/// `active_users` — daily-unique sign-ins, derived from `login_activities`
/// (Mastodon's Redis `activity:logins` unique tracker). `total`/`previous_total`
/// are the distinct-user counts over the window and the preceding one.
pub async fn active_users(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Measurement, DbError> {
    let total = active_users_total(pool, start, end).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = active_users_total(pool, ps, pe).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT count(DISTINCT login_activities.user_id) FROM login_activities
            WHERE login_activities.success
              AND date_trunc('day', login_activities.created_at AT TIME ZONE 'UTC')::date = axis.period
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

/// Distinct users with a successful sign-in inside the window — the measure
/// behind the dashboard series above, also consumed directly by nodeinfo's
/// `activeMonth`/`activeHalfyear`.
pub async fn active_users_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT user_id) AS "c!" FROM login_activities
           WHERE success
             AND created_at >= ($1 AT TIME ZONE 'UTC')::date
             AND created_at < (($2 AT TIME ZONE 'UTC')::date + 1)"#,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `languages` dimension — UI-locale breakdown of users active in the window
/// (signed in between `start` and `end`).
pub async fn dim_languages(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT locale AS "key", count(*) AS "value!"
        FROM users
        WHERE current_sign_in_at BETWEEN $1 AND $2
          AND locale IS NOT NULL
        GROUP BY locale
        ORDER BY count(*) DESC
        LIMIT $3
        "#,
        start,
        end,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// `sources` dimension — registration-app breakdown of users created in the
/// window. A `NULL` `created_by_application_id` (no signup attribution) groups
/// under the local web app.
pub async fn dim_sources(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    limit: Option<i64>,
) -> Result<Vec<DimRow>, DbError> {
    sqlx::query_as!(
        DimRow,
        r#"
        SELECT oauth_apps.name AS "key?", count(*) AS "value!"
        FROM users
        LEFT JOIN oauth_apps ON oauth_apps.id = users.created_by_application_id
        WHERE users.created_at BETWEEN $1 AND $2
        GROUP BY oauth_apps.name
        ORDER BY count(*) DESC
        LIMIT $3
        "#,
        start,
        end,
        limit,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

/// One `(cohort, retention_period)` cell of the retention report.
#[derive(Debug, Clone)]
pub struct RetentionCell {
    pub cohort_period: Date,
    pub retention_period: Date,
    pub value: i64,
    pub rate: f64,
}

/// Retention cohorts (Mastodon's `Admin::Metrics::Retention`): for each cohort
/// of users registered in a `frequency` bucket, how many were still signing in
/// by each later bucket, as a count + rate. `frequency` must be `day` or
/// `month` (validated by the caller). Rows come ordered by cohort then
/// retention period for grouping.
pub async fn retention(
    pool: &PgPool,
    start: Date,
    end: Date,
    frequency: &str,
) -> Result<Vec<RetentionCell>, DbError> {
    sqlx::query_as!(
        RetentionCell,
        r#"
        SELECT
            axis.cohort_period AS "cohort_period!",
            axis.retention_period AS "retention_period!",
            cnt.value AS "value!",
            cnt.rate AS "rate!"
        FROM (
            WITH cohort_periods AS (
                SELECT generate_series(
                    date_trunc($3, ($1::date)::timestamp)::date,
                    date_trunc($3, ($2::date)::timestamp)::date,
                    ('1 ' || $3)::interval
                )::date AS cohort_period
            ),
            retention_periods AS (
                SELECT cohort_period AS retention_period FROM cohort_periods
            )
            SELECT cohort_period, retention_period
            FROM cohort_periods, retention_periods
            WHERE retention_period >= cohort_period
        ) AS axis
        CROSS JOIN LATERAL (
            WITH new_users AS (
                SELECT users.id FROM users
                WHERE date_trunc($3, users.created_at AT TIME ZONE 'UTC')::date = axis.cohort_period
            ),
            retained_users AS (
                SELECT users.id FROM users
                INNER JOIN new_users ON new_users.id = users.id
                WHERE date_trunc($3, users.current_sign_in_at AT TIME ZONE 'UTC') >= axis.retention_period
            )
            SELECT
                count(*) AS value,
                (count(*))::float / (SELECT GREATEST(count(*), 1) FROM new_users) AS rate
            FROM retained_users
        ) AS cnt
        ORDER BY axis.cohort_period, axis.retention_period
        "#,
        start,
        end,
        frequency,
    )
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

// ---------------------------------------------------------------------------
// space_usage / software_versions backing queries (partial)
// ---------------------------------------------------------------------------

/// `pg_database_size(current_database())` — bytes, for the `space_usage`
/// dimension's `postgresql` line.
pub async fn pg_database_size(pool: &PgPool) -> Result<i64, DbError> {
    sqlx::query_scalar!(r#"SELECT pg_database_size(current_database()) AS "size!""#)
        .fetch_one(pool)
        .await
        .map_err(DbError::from)
}

// ---------------------------------------------------------------------------
// Interaction + media-byte-size metrics
// ---------------------------------------------------------------------------

/// Records one local interaction (favourite / reblog / follow / poll vote / new
/// status), bumping today's `daily_interactions` counter. Replaces Mastodon's
/// Redis `ActivityTracker.increment('activity:interactions')`. The day is the
/// current UTC date, consistent with the measures' UTC bucketing.
pub async fn record_interaction(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO daily_interactions (day, count)
        VALUES ((now() AT TIME ZONE 'UTC')::date, 1)
        ON CONFLICT (day) DO UPDATE SET count = daily_interactions.count + 1
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// `interactions` measure — local interactions per day from `daily_interactions`.
pub async fn interactions(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Measurement, DbError> {
    let total = interactions_total(pool, start, end).await?;
    let (ps, pe) = previous_window(start, end);
    let previous_total = interactions_total(pool, ps, pe).await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", COALESCE(di.count, 0) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        LEFT JOIN daily_interactions di ON di.day = axis.period
        "#,
        start,
        end,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: Some(previous_total),
        data,
    })
}

async fn interactions_total(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(count), 0)::bigint AS "c!" FROM daily_interactions
           WHERE day >= ($1 AT TIME ZONE 'UTC')::date
             AND day <= ($2 AT TIME ZONE 'UTC')::date"#,
        start,
        end,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// `instance_media_attachments` measure — bytes of media stored for `domain`,
/// per day. `total` is the all-time combined (file + thumbnail) size;
/// `previous_total` is `None` (Mastodon `total_in_time_range? == false`).
pub async fn instance_media_attachments(
    pool: &PgPool,
    start: OffsetDateTime,
    end: OffsetDateTime,
    domain: &str,
    include_subdomains: bool,
) -> Result<Measurement, DbError> {
    let total = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(
               COALESCE(media_attachments.file_size, 0)
             + COALESCE(media_attachments.thumbnail_file_size, 0)
           ), 0)::bigint AS "c!"
           FROM media_attachments
           INNER JOIN accounts ON accounts.id = media_attachments.account_id
           WHERE accounts.domain = $1 OR ($2 AND accounts.domain LIKE '%.' || $1)"#,
        domain,
        include_subdomains,
    )
    .fetch_one(pool)
    .await?;
    let data = sqlx::query_as!(
        SeriesPoint,
        r#"
        SELECT axis.period AS "period!", (
            SELECT COALESCE(SUM(
                   COALESCE(media_attachments.file_size, 0)
                 + COALESCE(media_attachments.thumbnail_file_size, 0)
               ), 0)::bigint
            FROM media_attachments
            INNER JOIN accounts ON accounts.id = media_attachments.account_id
            WHERE date_trunc('day', media_attachments.created_at AT TIME ZONE 'UTC')::date = axis.period
              AND (accounts.domain = $3 OR ($4 AND accounts.domain LIKE '%.' || $3))
        ) AS "value!"
        FROM (
            SELECT generate_series(
                ($1 AT TIME ZONE 'UTC')::date::timestamp,
                ($2 AT TIME ZONE 'UTC')::date::timestamp,
                '1 day'
            )::date AS period
        ) AS axis
        "#,
        start,
        end,
        domain,
        include_subdomains,
    )
    .fetch_all(pool)
    .await?;
    Ok(Measurement {
        total,
        previous_total: None,
        data,
    })
}

/// Total bytes of stored media for the `space_usage` dimension's `media` line:
/// attachments (file + thumbnail) + account avatars/headers + custom emojis +
/// preview-card images. Mastodon also sums backups/site-uploads (N/A here).
pub async fn media_storage_bytes(pool: &PgPool) -> Result<i64, DbError> {
    let row = sqlx::query_scalar!(
        r#"
        SELECT (
            (SELECT COALESCE(SUM(COALESCE(file_size, 0) + COALESCE(thumbnail_file_size, 0)), 0)::bigint FROM media_attachments)
          + (SELECT COALESCE(SUM(COALESCE(avatar_file_size, 0) + COALESCE(header_file_size, 0)), 0)::bigint FROM accounts)
          + (SELECT COALESCE(SUM(COALESCE(image_file_size, 0)), 0)::bigint FROM custom_emojis)
          + (SELECT COALESCE(SUM(COALESCE(image_file_size, 0)), 0)::bigint FROM preview_cards)
        ) AS "c!"
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// The server's Postgres version string (e.g. `17.2`), parsed from
/// `version()`, for the `software_versions` dimension's `postgresql` line.
pub async fn pg_version(pool: &PgPool) -> Result<String, DbError> {
    let full = sqlx::query_scalar!(r#"SELECT version() AS "v!""#)
        .fetch_one(pool)
        .await?;
    // "PostgreSQL 17.2 (Debian ...) on x86_64..." -> "17.2"
    let trimmed = full.strip_prefix("PostgreSQL ").unwrap_or(&full);
    Ok(trimmed
        .split_whitespace()
        .next()
        .unwrap_or(trimmed)
        .to_owned())
}
