//! Public discovery data: the peer-domain list behind
//! `/api/v1/instance/peers` + `/api/v1/peers/search`, the rolling weekly
//! activity behind `/api/v1/instance/activity`, the user-facing
//! domain-block list behind `/api/v1/instance/domain_blocks`, and the
//! profile directory behind `/api/v1/directory`.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// Mastodon's `Instance.searchable` set: every domain we know accounts from,
/// plus explicitly allowed domains, minus anything with a domain-block row
/// (any severity). Mastodon plucks the view unordered; we order by domain so
/// the response is stable.
pub async fn peer_domains(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let domains = sqlx::query_scalar!(
        r#"
        SELECT peers.domain AS "domain!"
        FROM (
            SELECT DISTINCT domain FROM accounts WHERE domain IS NOT NULL AND NOT portable
            UNION
            SELECT domain FROM domain_allows
        ) AS peers
        WHERE peers.domain NOT IN (SELECT domain FROM domain_blocks)
        ORDER BY peers.domain
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(domains)
}

/// Prefix search over [`peer_domains`], most-populated domains first —
/// Mastodon's non-Elasticsearch fallback with its `accounts_count` ranking
/// folded in. `prefix` is matched literally (LIKE metacharacters escaped).
pub async fn peer_domains_matching(
    pool: &PgPool,
    prefix: &str,
    limit: i64,
) -> Result<Vec<String>, DbError> {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let domains = sqlx::query_scalar!(
        r#"
        SELECT peers.domain AS "domain!"
        FROM (
            SELECT accounts.domain, count(*) AS accounts_count
            FROM accounts WHERE accounts.domain IS NOT NULL AND NOT accounts.portable
            GROUP BY accounts.domain
            UNION
            SELECT domain_allows.domain, 0 FROM domain_allows
            WHERE domain_allows.domain NOT IN
                (SELECT accounts.domain FROM accounts
                 WHERE accounts.domain IS NOT NULL AND NOT accounts.portable)
        ) AS peers
        WHERE peers.domain NOT IN (SELECT domain FROM domain_blocks)
          AND peers.domain LIKE $1 || '%'
        ORDER BY peers.accounts_count DESC, peers.domain
        LIMIT $2
        "#,
        escaped,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(domains)
}

/// Whether federation is allow-list based (any `domain_allows` rows) — the
/// analogue of Mastodon's `LIMITED_FEDERATION_MODE`, which hides the peers
/// and activity APIs.
pub async fn allow_list_active(pool: &PgPool) -> Result<bool, DbError> {
    let active =
        sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM domain_allows) AS "active!""#,)
            .fetch_one(pool)
            .await?;
    Ok(active)
}

/// One rolling week of instance activity, newest first.
pub struct ActivityWeek {
    /// The week's start (exactly `now - n weeks`, like Mastodon's
    /// `num.weeks.ago`).
    pub week_start: OffsetDateTime,
    /// Local statuses (boosts included — they are status rows) created in
    /// the week. Counted from live rows, so deletions subtract where
    /// Mastodon's Redis counter would not.
    pub statuses: i64,
    /// Distinct local users who signed in during the week.
    pub logins: i64,
    /// Local registrations during the week.
    pub registrations: i64,
}

/// The last 12 rolling weeks of activity, current week first — the data
/// behind `GET /api/v1/instance/activity`. Each week covers the 7 calendar
/// days (UTC) starting on the day `n` weeks ago, matching Mastodon's
/// day-bucketed `ActivityTracker` sums.
pub async fn activity_weeks(pool: &PgPool) -> Result<Vec<ActivityWeek>, DbError> {
    let weeks = sqlx::query_as!(
        ActivityWeek,
        r#"
        SELECT axis.week_start AS "week_start!",
               (SELECT count(*) FROM statuses
                WHERE statuses.uri IS NULL
                  AND statuses.deleted_at IS NULL -- STUBFILTER
                  AND (statuses.created_at AT TIME ZONE 'UTC')::date
                      BETWEEN axis.week_day AND axis.week_day + 6) AS "statuses!",
               (SELECT count(DISTINCT login_activities.user_id) FROM login_activities
                WHERE login_activities.success
                  AND (login_activities.created_at AT TIME ZONE 'UTC')::date
                      BETWEEN axis.week_day AND axis.week_day + 6) AS "logins!",
               (SELECT count(*) FROM users
                WHERE (users.created_at AT TIME ZONE 'UTC')::date
                      BETWEEN axis.week_day AND axis.week_day + 6) AS "registrations!"
        FROM (
            SELECT now() - make_interval(weeks => n) AS week_start,
                   ((now() - make_interval(weeks => n)) AT TIME ZONE 'UTC')::date
                       AS week_day
            FROM generate_series(0, 11) AS n
        ) AS axis
        ORDER BY axis.week_start DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(weeks)
}

/// One page of the profile directory.
pub struct DirectoryPage {
    /// `order=new` (newest accounts first); the default is `active`
    /// (most recently posted first).
    pub order_new: bool,
    /// `local=true` — hide remote accounts.
    pub local_only: bool,
    /// A signed-in viewer: accounts they block/mute (or that block them),
    /// plus domains they block, drop out of their directory.
    pub viewer_id: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

/// The account ids of one directory page — Mastodon's `Account.discoverable`
/// scope: opted in via `discoverable`, not suspended/silenced, not migrated
/// away, and (for local accounts) an approved user. Ordering ties break on
/// newest account so pages are stable.
pub async fn directory_account_ids(
    pool: &PgPool,
    page: &DirectoryPage,
) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT accounts.id
        FROM accounts
        LEFT JOIN users ON users.account_id = accounts.id
        WHERE accounts.discoverable IS TRUE
          AND accounts.suspended_at IS NULL
          AND accounts.silenced_at IS NULL
          AND accounts.moved_to_uri IS NULL
          -- Groups have their own directory (/groups); the people directory
          -- stays people (Mastodon's stays Person-only too).
          AND accounts.actor_type IS DISTINCT FROM 'Group'
          AND (users.id IS NULL OR (users.approved AND users.confirmed_at IS NOT NULL))
          AND (NOT $1::boolean OR accounts.domain IS NULL OR accounts.portable)
          AND ($2::bigint IS NULL OR (
                accounts.id NOT IN (
                    SELECT target_account_id FROM blocks WHERE account_id = $2
                    UNION
                    SELECT account_id FROM blocks WHERE target_account_id = $2
                    UNION
                    SELECT target_account_id FROM mutes WHERE account_id = $2
                      AND (expires_at IS NULL OR expires_at > now())
                )
            AND (accounts.portable OR accounts.domain IS NULL OR accounts.domain NOT IN (
                    SELECT domain FROM account_domain_blocks WHERE account_id = $2
                ))
          ))
        ORDER BY CASE WHEN $3::boolean THEN accounts.id END DESC,
                 (SELECT max(statuses.created_at) FROM statuses
                  WHERE statuses.account_id = accounts.id
                    AND statuses.deleted_at IS NULL /* STUBFILTER */) DESC NULLS LAST,
                 accounts.id DESC
        LIMIT $4 OFFSET $5
        "#,
        page.local_only,
        page.viewer_id,
        page.order_new,
        page.limit,
        page.offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// A domain block as disclosed on `GET /api/v1/instance/domain_blocks`.
pub struct PublicDomainBlock {
    pub domain: String,
    pub severity: String,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
}

/// Mastodon's `DomainBlock.with_user_facing_limitations.by_severity`: the
/// silenced and suspended domains, silence before suspend, then by domain.
pub async fn user_facing_domain_blocks(pool: &PgPool) -> Result<Vec<PublicDomainBlock>, DbError> {
    let blocks = sqlx::query_as!(
        PublicDomainBlock,
        r#"
        SELECT domain, severity, public_comment, obfuscate
        FROM domain_blocks
        WHERE severity IN ('silence', 'suspend')
        ORDER BY CASE severity WHEN 'silence' THEN 1 ELSE 2 END, domain
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(blocks)
}
