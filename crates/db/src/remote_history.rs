//! Durable remote-profile history state and leased work queue.
//!
//! This module intentionally knows nothing about HTTP or `ActivityStreams`. It
//! owns admission, coalescing, crash recovery, backoff, durable cursors, and
//! conservative retention; the server worker owns guarded federation fetches
//! and cold ingest.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use crate::{DbError, id, media_cleanup, retention_sweep};

const USER_REQUESTS_PER_HOUR: i32 = 30;
const ORIGIN_QUEUE_DEPTH: i64 = 2;
const FRESH_MINUTES: i64 = 60;
const AUTOMATIC_COOLDOWN_HOURS: i64 = 6;
const UNSUPPORTED_COOLDOWN_DAYS: i64 = 7;
const LEASE_SECONDS: f64 = 120.0;
const MAX_ATTEMPTS: i32 = 5;

#[derive(Debug, Clone)]
pub struct Settings {
    pub enabled: bool,
    pub retention_days: i32,
    pub bare_iri_enabled: bool,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub account_id: i64,
    pub hydration_enabled: bool,
    pub outbox_uri: Option<String>,
    pub first_page_uri: Option<String>,
    pub next_page_uri: Option<String>,
    pub state: String,
    pub reported_total_items: Option<i64>,
    pub outbox_etag: Option<String>,
    pub first_page_etag: Option<String>,
    pub anonymous_visibility: String,
    pub last_attempt_at: Option<OffsetDateTime>,
    pub last_success_at: Option<OffsetDateTime>,
    pub retry_at: Option<OffsetDateTime>,
    pub automatic_retry_at: Option<OffsetDateTime>,
    pub last_error_class: Option<String>,
    pub pages_fetched: i64,
    pub items_seen: i64,
    pub items_accepted: i64,
    pub bytes_fetched: i64,
    pub available_statuses: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Initial,
    Refresh,
    Older,
}

impl JobKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Refresh => "refresh",
            Self::Older => "older",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: i64,
    pub account_id: i64,
    pub kind: String,
    pub page_uri: Option<String>,
    pub origin: String,
    pub requested_by: Option<i64>,
    pub attempts: i32,
    pub created_at: OffsetDateTime,
}

impl ClaimedJob {
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.attempts > MAX_ATTEMPTS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued,
    Coalesced,
    Fresh,
    Disabled,
    Backoff(OffsetDateTime),
    AutomaticCooldown(OffsetDateTime),
    OriginBusy(OffsetDateTime),
    RateLimited(OffsetDateTime),
}

/// Whether admission came from a passive profile/timeline view or an explicit
/// fetch control. Automatic hints are durably cooled down; explicit actions
/// retain the existing freshness, origin-backoff, and user-budget gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Automatic,
    Explicit,
}

#[derive(Debug, Clone)]
pub struct Success {
    pub page_uri: Option<String>,
    pub first_page_uri: Option<String>,
    pub next_page_uri: Option<String>,
    pub outbox_etag: Option<String>,
    pub first_page_etag: Option<String>,
    pub reported_total_items: Option<i64>,
    pub pages: u64,
    pub items_seen: u64,
    pub items_accepted: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct OriginDiagnostic {
    pub origin: String,
    pub queued: i64,
    pub oldest_job_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct OriginStorageDiagnostic {
    pub origin: String,
    pub statuses: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone)]
pub struct FailureDiagnostic {
    pub class: String,
    pub actors: i64,
}

#[derive(Debug, Clone)]
pub struct Diagnostics {
    pub pending_jobs: i64,
    pub leased_jobs: i64,
    pub history_statuses: i64,
    pub history_bytes: i64,
    pub top_origins: Vec<OriginDiagnostic>,
    pub top_storage_origins: Vec<OriginStorageDiagnostic>,
    pub failures: Vec<FailureDiagnostic>,
}

pub async fn settings(pool: &PgPool) -> Result<Settings, DbError> {
    Ok(sqlx::query_as!(
        Settings,
        "SELECT enabled, retention_days, bare_iri_enabled, updated_at
         FROM remote_history_settings WHERE singleton",
    )
    .fetch_one(pool)
    .await?)
}

pub async fn save_settings(
    pool: &PgPool,
    enabled: bool,
    retention_days: i32,
    bare_iri_enabled: bool,
) -> Result<Settings, DbError> {
    if !(1..=3650).contains(&retention_days) {
        return Err(DbError::Protocol(
            "remote history retention must be between 1 and 3650 days".to_owned(),
        ));
    }
    Ok(sqlx::query_as!(
        Settings,
        r#"UPDATE remote_history_settings SET
               enabled = $1, retention_days = $2, bare_iri_enabled = $3,
               updated_at = now()
           WHERE singleton
           RETURNING enabled, retention_days, bare_iri_enabled, updated_at"#,
        enabled,
        retention_days,
        bare_iri_enabled,
    )
    .fetch_one(pool)
    .await?)
}

/// Mirrors actor collection metadata and the two `GoToSocial` unauthenticated
/// web visibility hints. `None` means the actor did not make a statement.
pub async fn set_actor_metadata(
    pool: &PgPool,
    account_id: i64,
    outbox_uri: Option<&str>,
    restrict_anonymous: Option<bool>,
) -> Result<(), DbError> {
    let policy = restrict_anonymous.map(|value| if value { "restricted" } else { "allowed" });
    sqlx::query!(
        r#"INSERT INTO remote_history_states
               (account_id, outbox_uri, anonymous_visibility)
           VALUES ($1, $2, COALESCE($3, 'unknown'))
           ON CONFLICT (account_id) DO UPDATE SET
               state = CASE
                   WHEN remote_history_states.outbox_uri IS DISTINCT FROM excluded.outbox_uri
                       AND NOT EXISTS (
                           SELECT 1 FROM remote_history_jobs
                           WHERE account_id = remote_history_states.account_id
                       )
                       THEN 'idle'
                   ELSE remote_history_states.state
               END,
               retry_at = CASE
                   WHEN remote_history_states.outbox_uri IS DISTINCT FROM excluded.outbox_uri
                       THEN NULL
                   ELSE remote_history_states.retry_at
               END,
               automatic_retry_at = CASE
                   WHEN remote_history_states.outbox_uri IS DISTINCT FROM excluded.outbox_uri
                       THEN NULL
                   ELSE remote_history_states.automatic_retry_at
               END,
               last_error_class = CASE
                   WHEN remote_history_states.outbox_uri IS DISTINCT FROM excluded.outbox_uri
                       THEN NULL
                   ELSE remote_history_states.last_error_class
               END,
               outbox_uri = excluded.outbox_uri,
               anonymous_visibility = COALESCE($3, remote_history_states.anonymous_visibility),
               updated_at = now()"#,
        account_id,
        outbox_uri,
        policy,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn snapshot(pool: &PgPool, account_id: i64) -> Result<Option<Snapshot>, DbError> {
    let row = sqlx::query!(
        r#"SELECT h.account_id, rs.enabled AS "hydration_enabled!",
                  h.outbox_uri, h.first_page_uri, h.next_page_uri,
                  h.state, h.reported_total_items, h.outbox_etag, h.first_page_etag,
                  h.anonymous_visibility,
                  h.last_attempt_at, h.last_success_at, h.retry_at,
                  h.automatic_retry_at,
                  h.last_error_class, h.pages_fetched, h.items_seen,
                  h.items_accepted, h.bytes_fetched,
                  count(s.id) FILTER (
                      WHERE s.deleted_at IS NULL
                        -- STUBFILTER
                        AND s.visibility IN ('public', 'unlisted')
                  ) AS "available_statuses!"
           FROM remote_history_states h
           CROSS JOIN remote_history_settings rs
           LEFT JOIN statuses s ON s.account_id = h.account_id
           WHERE h.account_id = $1
           GROUP BY h.account_id, rs.enabled"#,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| Snapshot {
        account_id: row.account_id,
        hydration_enabled: row.hydration_enabled,
        outbox_uri: row.outbox_uri,
        first_page_uri: row.first_page_uri,
        next_page_uri: row.next_page_uri,
        state: row.state,
        reported_total_items: row.reported_total_items,
        outbox_etag: row.outbox_etag,
        first_page_etag: row.first_page_etag,
        anonymous_visibility: row.anonymous_visibility,
        last_attempt_at: row.last_attempt_at,
        last_success_at: row.last_success_at,
        retry_at: row.retry_at,
        automatic_retry_at: row.automatic_retry_at,
        last_error_class: row.last_error_class,
        pages_fetched: row.pages_fetched,
        items_seen: row.items_seen,
        items_accepted: row.items_accepted,
        bytes_fetched: row.bytes_fetched,
        available_statuses: row.available_statuses,
    }))
}

/// Marks the actor as recently viewed. This is a local-only write; opening a
/// profile never waits on the network.
pub async fn touch_viewed(pool: &PgPool, account_id: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE remote_history_states SET last_viewed_at = now(), updated_at = now()
         WHERE account_id = $1",
        account_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Admits one logical request. Existing work is checked before the durable
/// per-user budget, so repeated clicks on the same job are free and coalesce.
#[allow(
    clippy::too_many_lines,
    reason = "one transaction owns every admission gate"
)]
pub async fn enqueue(
    pool: &PgPool,
    account_id: i64,
    kind: JobKind,
    page_uri: Option<&str>,
    origin: &str,
    requested_by: Option<i64>,
    admission: Admission,
) -> Result<EnqueueOutcome, DbError> {
    let mut tx = pool.begin().await?;
    let enabled = sqlx::query_scalar!(
        r#"SELECT enabled AS "enabled!" FROM remote_history_settings
           WHERE singleton FOR SHARE"#,
    )
    .fetch_one(&mut *tx)
    .await?;
    if !enabled {
        tx.rollback().await?;
        return Ok(EnqueueOutcome::Disabled);
    }

    let state = sqlx::query!(
        r#"SELECT last_success_at, retry_at, automatic_retry_at
           FROM remote_history_states WHERE account_id = $1 FOR UPDATE"#,
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(state) = state else {
        tx.rollback().await?;
        return Err(DbError::Protocol(
            "remote actor has no outbox metadata".to_owned(),
        ));
    };

    if sqlx::query_scalar!(
        "SELECT id FROM remote_history_jobs WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .is_some()
    {
        tx.commit().await?;
        return Ok(EnqueueOutcome::Coalesced);
    }
    if kind != JobKind::Older
        && state
            .last_success_at
            .is_some_and(|at| OffsetDateTime::now_utc() - at < Duration::minutes(FRESH_MINUTES))
    {
        tx.commit().await?;
        return Ok(EnqueueOutcome::Fresh);
    }
    if let Some(retry_at) = state.retry_at
        && retry_at > OffsetDateTime::now_utc()
    {
        tx.commit().await?;
        return Ok(EnqueueOutcome::Backoff(retry_at));
    }
    if admission == Admission::Automatic
        && let Some(retry_at) = state.automatic_retry_at
        && retry_at > OffsetDateTime::now_utc()
    {
        tx.commit().await?;
        return Ok(EnqueueOutcome::AutomaticCooldown(retry_at));
    }

    let origin_jobs = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM remote_history_jobs WHERE origin = $1"#,
        origin,
    )
    .fetch_one(&mut *tx)
    .await?;
    if origin_jobs >= ORIGIN_QUEUE_DEPTH {
        tx.commit().await?;
        return Ok(EnqueueOutcome::OriginBusy(
            OffsetDateTime::now_utc() + Duration::minutes(2),
        ));
    }

    if let Some(requester) = requested_by {
        let now = OffsetDateTime::now_utc();
        let period_index = now.unix_timestamp() / 3600;
        let expires_at = OffsetDateTime::from_unix_timestamp((period_index + 1) * 3600)
            .unwrap_or(now + Duration::hours(1));
        let count = sqlx::query_scalar!(
            r#"INSERT INTO rate_limit_windows
                   (bucket, identity, period_index, count, expires_at)
               VALUES ('remote_history', $1, $2, 1, $3)
               ON CONFLICT (bucket, identity) DO UPDATE SET
                   count = CASE
                       WHEN rate_limit_windows.period_index = excluded.period_index
                           THEN rate_limit_windows.count + 1
                       ELSE 1
                   END,
                   period_index = excluded.period_index,
                   expires_at = excluded.expires_at
               RETURNING count"#,
            requester.to_string(),
            period_index,
            expires_at,
        )
        .fetch_one(&mut *tx)
        .await?;
        if count > USER_REQUESTS_PER_HOUR {
            tx.commit().await?;
            return Ok(EnqueueOutcome::RateLimited(expires_at));
        }
    }

    sqlx::query!(
        r#"INSERT INTO remote_history_jobs
               (id, account_id, kind, page_uri, origin, requested_by)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
        id::next(),
        account_id,
        kind.as_str(),
        page_uri,
        origin,
        requested_by,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"UPDATE remote_history_states SET
               state = 'queued', retry_at = NULL, last_error_class = NULL,
               automatic_retry_at = CASE WHEN $2 THEN
                   $3
                   ELSE automatic_retry_at END,
               updated_at = now()
           WHERE account_id = $1"#,
        account_id,
        admission == Admission::Automatic,
        OffsetDateTime::now_utc() + Duration::hours(AUTOMATIC_COOLDOWN_HOURS),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(EnqueueOutcome::Enqueued)
}

/// Claims at most one due job per origin in a batch. Combined with the
/// server's keyed origin semaphore, this is fair across hosts and keeps the
/// persisted lease crash-safe.
#[allow(
    clippy::too_many_lines,
    reason = "job and origin leases are claimed atomically"
)]
pub async fn claim_due(
    pool: &PgPool,
    worker: &str,
    limit: i64,
) -> Result<Vec<ClaimedJob>, DbError> {
    if !settings(pool).await?.enabled {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"UPDATE remote_history_jobs SET
               state = 'pending', lease_owner = NULL, leased_until = NULL,
               run_at = now(), updated_at = now()
           WHERE state = 'leased' AND leased_until <= now()"#,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM remote_history_origin_leases WHERE leased_until <= now()",)
        .execute(&mut *tx)
        .await?;
    // Lock the oldest due job for each origin. The anti-join deliberately sees
    // a locked earlier row, so a second process cannot skip it and claim the
    // same origin's next job during a rolling deploy.
    let candidates = sqlx::query!(
        r#"SELECT j.id, j.origin
           FROM remote_history_jobs j
           WHERE j.state = 'pending' AND j.run_at <= now()
             AND NOT EXISTS (
                 SELECT 1 FROM remote_history_origin_leases l
                 WHERE l.origin = j.origin AND l.leased_until > now()
             )
             AND NOT EXISTS (
                 SELECT 1 FROM remote_history_jobs earlier
                 WHERE earlier.origin = j.origin
                   AND earlier.state = 'pending' AND earlier.run_at <= now()
                   AND (earlier.run_at, earlier.created_at, earlier.id)
                       < (j.run_at, j.created_at, j.id)
             )
           ORDER BY j.run_at, j.created_at
           LIMIT $1
           FOR UPDATE OF j SKIP LOCKED"#,
        limit,
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut jobs = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let origin_claimed = sqlx::query_scalar!(
            r#"INSERT INTO remote_history_origin_leases
                   (origin, job_id, lease_owner, leased_until)
               VALUES ($1, $2, $3, now() + make_interval(secs => $4))
               ON CONFLICT (origin) DO UPDATE SET
                   job_id = excluded.job_id,
                   lease_owner = excluded.lease_owner,
                   leased_until = excluded.leased_until
               WHERE remote_history_origin_leases.leased_until <= now()
               RETURNING origin"#,
            candidate.origin,
            candidate.id,
            worker,
            LEASE_SECONDS,
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !origin_claimed {
            continue;
        }
        let claimed = sqlx::query_as!(
            ClaimedJob,
            r#"UPDATE remote_history_jobs SET
                   state = 'leased', lease_owner = $2,
                   leased_until = now() + make_interval(secs => $3),
                   attempts = attempts + 1, updated_at = now()
               WHERE id = $1 AND state = 'pending'
               RETURNING id, account_id, kind, page_uri, origin,
                         requested_by, attempts, created_at"#,
            candidate.id,
            worker,
            LEASE_SECONDS,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(claimed) = claimed {
            jobs.push(claimed);
        } else {
            sqlx::query!(
                "DELETE FROM remote_history_origin_leases WHERE job_id = $1",
                candidate.id,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    if !jobs.is_empty() {
        let account_ids: Vec<i64> = jobs.iter().map(|job| job.account_id).collect();
        sqlx::query!(
            r#"UPDATE remote_history_states SET
                   state = 'fetching', last_attempt_at = now(), updated_at = now()
               WHERE account_id = ANY($1)"#,
            &account_ids,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(jobs)
}

/// Returns `false` for a page already fetched for this actor.
pub async fn record_page(pool: &PgPool, account_id: i64, uri: &str) -> Result<bool, DbError> {
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO remote_history_pages (account_id, page_uri)
           VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING account_id"#,
        account_id,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

pub async fn page_seen(pool: &PgPool, account_id: i64, uri: &str) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM remote_history_pages
                       WHERE account_id = $1 AND page_uri = $2) AS \"seen!\"",
        account_id,
        uri,
    )
    .fetch_one(pool)
    .await?)
}

pub async fn finish_success(
    pool: &PgPool,
    job: &ClaimedJob,
    result: &Success,
) -> Result<(), DbError> {
    let terminal_state = if result.next_page_uri.is_some() {
        "partial"
    } else {
        "complete"
    };
    let mut tx = pool.begin().await?;
    if let Some(page_uri) = result.page_uri.as_deref() {
        sqlx::query!(
            r#"INSERT INTO remote_history_pages (account_id, page_uri)
               VALUES ($1, $2) ON CONFLICT DO NOTHING"#,
            job.account_id,
            page_uri,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        r#"UPDATE remote_history_states SET
               first_page_uri = COALESCE($2, first_page_uri),
               next_page_uri = $3,
               outbox_etag = COALESCE($4, outbox_etag),
               first_page_etag = COALESCE($5, first_page_etag),
               reported_total_items = COALESCE($6, reported_total_items),
               state = $7, last_success_at = now(), retry_at = NULL,
               automatic_retry_at = $12,
               last_error_class = NULL,
               pages_fetched = pages_fetched + $8,
               items_seen = items_seen + $9,
               items_accepted = items_accepted + $10,
               bytes_fetched = bytes_fetched + $11,
               updated_at = now()
           WHERE account_id = $1"#,
        job.account_id,
        result.first_page_uri.as_deref(),
        result.next_page_uri.as_deref(),
        result.outbox_etag.as_deref(),
        result.first_page_etag.as_deref(),
        result.reported_total_items,
        terminal_state,
        i64::try_from(result.pages).unwrap_or(i64::MAX),
        i64::try_from(result.items_seen).unwrap_or(i64::MAX),
        i64::try_from(result.items_accepted).unwrap_or(i64::MAX),
        i64::try_from(result.bytes).unwrap_or(i64::MAX),
        OffsetDateTime::now_utc() + Duration::hours(AUTOMATIC_COOLDOWN_HOURS),
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM remote_history_jobs WHERE id = $1", job.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Finishes an attempt. Retryable failures retain the row with a durable
/// `run_at`; terminal failures remove it and leave an honest state for the UI.
pub async fn finish_failure(
    pool: &PgPool,
    job: &ClaimedJob,
    error_class: &str,
    retry_at: Option<OffsetDateTime>,
    unsupported: bool,
) -> Result<(), DbError> {
    let retry = !unsupported && !job.exhausted();
    let retry_at = retry_at.unwrap_or_else(|| {
        OffsetDateTime::now_utc() + Duration::minutes(i64::from(job.attempts.clamp(1, 30)))
    });
    let mut tx = pool.begin().await?;
    if retry {
        sqlx::query!(
            r#"UPDATE remote_history_jobs SET
                   state = 'pending', run_at = $2, lease_owner = NULL,
                   leased_until = NULL, updated_at = now()
               WHERE id = $1"#,
            job.id,
            retry_at,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM remote_history_origin_leases WHERE job_id = $1",
            job.id,
        )
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query!("DELETE FROM remote_history_jobs WHERE id = $1", job.id)
            .execute(&mut *tx)
            .await?;
    }
    let state = if unsupported {
        "unsupported"
    } else if retry {
        "backoff"
    } else {
        "partial"
    };
    sqlx::query!(
        r#"UPDATE remote_history_states SET
               state = $2, retry_at = $3, last_error_class = $4,
               automatic_retry_at = CASE
                   WHEN $5 THEN $6
                   WHEN NOT $7 THEN $8
                   ELSE automatic_retry_at
               END,
               updated_at = now()
           WHERE account_id = $1"#,
        job.account_id,
        state,
        retry.then_some(retry_at),
        error_class,
        unsupported,
        OffsetDateTime::now_utc() + Duration::days(UNSUPPORTED_COOLDOWN_DAYS),
        retry,
        OffsetDateTime::now_utc() + Duration::hours(AUTOMATIC_COOLDOWN_HOURS),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM remote_history_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub async fn diagnostics(pool: &PgPool) -> Result<Diagnostics, DbError> {
    let totals = sqlx::query!(
        r#"SELECT
               count(*) FILTER (WHERE state = 'pending') AS "pending!",
               count(*) FILTER (WHERE state = 'leased') AS "leased!"
           FROM remote_history_jobs"#,
    )
    .fetch_one(pool)
    .await?;
    let storage = sqlx::query!(
        r#"SELECT count(*) AS "statuses!",
                  COALESCE(sum(octet_length(content) + octet_length(spoiler_text)), 0)::bigint
                      AS "bytes!"
           FROM statuses WHERE ingest_provenance = 'history'
             AND deleted_at IS NULL -- STUBFILTER"#,
    )
    .fetch_one(pool)
    .await?;
    let top_origins = sqlx::query_as!(
        OriginDiagnostic,
        r#"SELECT origin, count(*) AS "queued!", min(created_at) AS "oldest_job_at!"
           FROM remote_history_jobs GROUP BY origin
           ORDER BY count(*) DESC, min(created_at) LIMIT 10"#,
    )
    .fetch_all(pool)
    .await?;
    let top_storage_origins = sqlx::query_as!(
        OriginStorageDiagnostic,
        r#"SELECT a.domain AS "origin!", count(*) AS "statuses!",
                  COALESCE(sum(octet_length(s.content) + octet_length(s.spoiler_text)), 0)::bigint
                      AS "bytes!"
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.ingest_provenance = 'history'
             AND s.deleted_at IS NULL -- STUBFILTER
             AND a.domain IS NOT NULL
           GROUP BY a.domain ORDER BY count(*) DESC LIMIT 10"#,
    )
    .fetch_all(pool)
    .await?;
    let failures = sqlx::query_as!(
        FailureDiagnostic,
        r#"SELECT last_error_class AS "class!", count(*) AS "actors!"
           FROM remote_history_states WHERE last_error_class IS NOT NULL
           GROUP BY last_error_class ORDER BY count(*) DESC"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(Diagnostics {
        pending_jobs: totals.pending,
        leased_jobs: totals.leased,
        history_statuses: storage.statuses,
        history_bytes: storage.bytes,
        top_origins,
        top_storage_origins,
        failures,
    })
}

/// Removes a small batch of stale cold rows, preserving anything a local user
/// has interacted with or that anchors a local reply/reblog/quote/moderation
/// reference. Media keys are captured and queued atomically with deletion.
pub async fn prune(pool: &PgPool, retention_days: i32, limit: i64) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    let ids = sqlx::query_scalar!(
        r#"SELECT s.id
           FROM statuses s
           LEFT JOIN remote_history_states h ON h.account_id = s.account_id
           WHERE s.ingest_provenance = 'history'
             AND s.deleted_at IS NULL -- STUBFILTER
             AND COALESCE(s.history_last_touched_at, s.history_fetched_at, s.created_at)
                   < now() - make_interval(days => $1)
             AND COALESCE(h.last_viewed_at, '-infinity'::timestamptz)
                   < now() - make_interval(days => $1)
             AND NOT EXISTS (SELECT 1 FROM bookmarks b WHERE b.status_id = s.id)
             AND NOT EXISTS (SELECT 1 FROM favourites f WHERE f.status_id = s.id)
             AND NOT EXISTS (SELECT 1 FROM status_pins p WHERE p.status_id = s.id)
             AND NOT EXISTS (
                 SELECT 1 FROM status_reactions r JOIN accounts a ON a.id = r.account_id
                 WHERE r.status_id = s.id AND a.domain IS NULL
             )
             AND NOT EXISTS (
                 SELECT 1 FROM status_dislikes d JOIN accounts a ON a.id = d.account_id
                 WHERE d.status_id = s.id AND a.domain IS NULL
             )
             AND NOT EXISTS (
                 SELECT 1 FROM polls p JOIN poll_votes v ON v.poll_id = p.id
                 JOIN accounts a ON a.id = v.account_id
                 WHERE p.status_id = s.id AND a.domain IS NULL
             )
             AND NOT EXISTS (
                 SELECT 1 FROM statuses child JOIN accounts a ON a.id = child.account_id
                 WHERE (child.in_reply_to_id = s.id OR child.reblog_of_id = s.id)
                   AND a.domain IS NULL
             )
             AND NOT EXISTS (
                 SELECT 1 FROM quotes q
                 WHERE q.status_id = s.id OR q.quoted_status_id = s.id
             )
             AND NOT EXISTS (
                 SELECT 1 FROM reports r WHERE s.id = ANY(r.status_ids)
             )
           ORDER BY COALESCE(s.history_last_touched_at, s.history_fetched_at, s.created_at), s.id
           LIMIT $2
           FOR UPDATE OF s SKIP LOCKED"#,
        retention_days,
        limit,
    )
    .fetch_all(&mut *tx)
    .await?;
    let keys = media_cleanup::collect_status_keys_many(&mut tx, &ids).await?;
    let deleted = if ids.is_empty() {
        0
    } else {
        sqlx::query!(
            "DELETE FROM statuses WHERE id = ANY($1) AND ingest_provenance = 'history'",
            &ids,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };
    media_cleanup::enqueue_many(&mut *tx, &keys).await?;
    tx.commit().await?;
    retention_sweep::record(pool, "remote_history", deleted, 0).await?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    #[sqlx::test]
    async fn fresh_history_defaults_can_be_disabled(pool: PgPool) {
        let initial = settings(&pool).await.unwrap();
        assert!(initial.enabled);
        assert!(initial.bare_iri_enabled);
        let saved = save_settings(&pool, false, initial.retention_days, false)
            .await
            .unwrap();
        assert!(!saved.enabled);
        assert!(!saved.bare_iri_enabled);
    }

    use super::*;
    use crate::account::{self, NewLocalAccount, RemoteAccountData};
    use crate::status::{self, IngestProvenance, NewLocalStatus, NewRemoteStatus};

    async fn remote_named(pool: &PgPool, username: &str, domain: &str) -> i64 {
        let uri = format!("https://{domain}/users/{username}");
        account::upsert_remote(
            pool,
            RemoteAccountData {
                username,
                domain,
                uri: &uri,
                display_name: username,
                note: "",
                inbox_url: &format!("{uri}/inbox"),
                shared_inbox_url: "",
                public_key_pem: "pub",
                public_key_id: &format!("{uri}#main-key"),
                avatar_remote_url: None,
                header_remote_url: None,
                avatar_description: "",
                header_description: "",
                created_at: None,
                fields: Vec::new(),
                featured_collection_url: None,
                locked: false,
                also_known_as: &[],
                moved_to_uri: None,
                url: None,
                discoverable: false,
                feature_approval_policy: 0,
                is_bot: false,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                actor_type: Some("Person"),
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn cold_status(pool: &PgPool, remote: i64, marker: &str) -> status::Status {
        let uri = format!("https://remote.example/users/bob/statuses/{marker}");
        status::upsert_remote_with_provenance(
            pool,
            NewRemoteStatus {
                uri: &uri,
                account_id: remote,
                content: marker,
                created_at: OffsetDateTime::now_utc() - Duration::days(120),
                visibility: "public",
                in_reply_to_id: None,
                in_reply_to_uri: None,
                spoiler_text: "",
                sensitive: false,
                language: None,
                url: None,
                quote_approval_policy: 0,
                title: None,
                object_type: None,
                external_url: None,
            },
            IngestProvenance::History,
        )
        .await
        .unwrap()
    }

    async fn actors(pool: &PgPool) -> (i64, i64) {
        let local = account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id;
        let remote = remote_named(pool, "bob", "remote.example").await;
        (local, remote)
    }

    #[sqlx::test]
    async fn queue_honours_disabled_setting_then_coalesces_and_leases(pool: PgPool) {
        save_settings(&pool, false, 90, false).await.unwrap();
        let (local, remote) = actors(&pool).await;
        set_actor_metadata(
            &pool,
            remote,
            Some("https://remote.example/users/bob/outbox"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap(),
            EnqueueOutcome::Disabled
        );
        save_settings(&pool, true, 90, false).await.unwrap();
        assert_eq!(
            enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap(),
            EnqueueOutcome::Enqueued
        );
        assert_eq!(
            enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap(),
            EnqueueOutcome::Coalesced
        );
        let claimed = claim_due(&pool, "test-worker", 4).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].account_id, remote);
        assert!(
            claim_due(&pool, "other-worker", 4)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn page_cycle_detection_survives_calls(pool: PgPool) {
        let (_, remote) = actors(&pool).await;
        set_actor_metadata(&pool, remote, Some("https://remote.example/outbox"), None)
            .await
            .unwrap();
        assert!(
            record_page(&pool, remote, "https://remote.example/page/1")
                .await
                .unwrap()
        );
        assert!(
            !record_page(&pool, remote, "https://remote.example/page/1")
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    async fn expired_job_and_origin_leases_are_recovered(pool: PgPool) {
        let (_, remote) = actors(&pool).await;
        set_actor_metadata(&pool, remote, Some("https://remote.example/outbox"), None)
            .await
            .unwrap();
        save_settings(&pool, true, 90, false).await.unwrap();
        enqueue(
            &pool,
            remote,
            JobKind::Initial,
            None,
            "https://remote.example",
            None,
            Admission::Explicit,
        )
        .await
        .unwrap();
        let first = claim_due(&pool, "worker-before-crash", 4).await.unwrap();
        assert_eq!(first.len(), 1);
        sqlx::query!(
            "UPDATE remote_history_jobs SET leased_until = now() - interval '1 second'
             WHERE id = $1",
            first[0].id,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE remote_history_origin_leases SET leased_until = now() - interval '1 second'
             WHERE job_id = $1",
            first[0].id,
        )
        .execute(&pool)
        .await
        .unwrap();
        let recovered = claim_due(&pool, "worker-after-crash", 4).await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, first[0].id);
        assert_eq!(recovered[0].attempts, 2);
    }

    #[sqlx::test]
    async fn retry_and_authorization_outcomes_survive_worker_turnover(pool: PgPool) {
        let (_, remote) = actors(&pool).await;
        set_actor_metadata(&pool, remote, Some("https://remote.example/outbox"), None)
            .await
            .unwrap();
        save_settings(&pool, true, 90, false).await.unwrap();
        enqueue(
            &pool,
            remote,
            JobKind::Initial,
            None,
            "https://remote.example",
            None,
            Admission::Explicit,
        )
        .await
        .unwrap();
        let job = claim_due(&pool, "worker-a", 1).await.unwrap().remove(0);
        let retry_at = OffsetDateTime::now_utc() + Duration::minutes(10);
        finish_failure(&pool, &job, "rate_limited", Some(retry_at), false)
            .await
            .unwrap();
        let backoff_state = snapshot(&pool, remote).await.unwrap().unwrap();
        assert_eq!(backoff_state.state, "backoff");
        assert_eq!(
            backoff_state.last_error_class.as_deref(),
            Some("rate_limited")
        );
        assert!(
            backoff_state
                .retry_at
                .is_some_and(|at| at >= retry_at - Duration::milliseconds(1)),
            "PostgreSQL timestamp precision may round the requested instant by less than 1 ms"
        );
        assert!(claim_due(&pool, "worker-b", 1).await.unwrap().is_empty());

        sqlx::query!(
            "UPDATE remote_history_jobs SET run_at = now() WHERE account_id = $1",
            remote,
        )
        .execute(&pool)
        .await
        .unwrap();
        let retried = claim_due(&pool, "worker-after-restart", 1)
            .await
            .unwrap()
            .remove(0);
        finish_failure(&pool, &retried, "unavailable", None, true)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
        let terminal = snapshot(&pool, remote).await.unwrap().unwrap();
        assert_eq!(terminal.state, "unsupported");
        assert_eq!(terminal.last_error_class.as_deref(), Some("unavailable"));
    }

    #[sqlx::test]
    async fn automatic_terminal_result_cools_down_without_blocking_explicit_retry(pool: PgPool) {
        let (local, remote) = actors(&pool).await;
        let outbox = "https://remote.example/outbox";
        set_actor_metadata(&pool, remote, Some(outbox), None)
            .await
            .unwrap();
        save_settings(&pool, true, 90, false).await.unwrap();

        assert_eq!(
            enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Automatic,
            )
            .await
            .unwrap(),
            EnqueueOutcome::Enqueued
        );
        let job = claim_due(&pool, "automatic-worker", 1)
            .await
            .unwrap()
            .remove(0);
        finish_failure(&pool, &job, "empty_collection", None, true)
            .await
            .unwrap();

        let retry_at = match enqueue(
            &pool,
            remote,
            JobKind::Initial,
            None,
            "https://remote.example",
            Some(local),
            Admission::Automatic,
        )
        .await
        .unwrap()
        {
            EnqueueOutcome::AutomaticCooldown(retry_at) => retry_at,
            outcome => panic!("terminal automatic admission was not cooled down: {outcome:?}"),
        };
        assert!(retry_at > OffsetDateTime::now_utc() + Duration::days(6));
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
        assert_eq!(
            snapshot(&pool, remote).await.unwrap().unwrap().state,
            "unsupported"
        );

        assert_eq!(
            enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap(),
            EnqueueOutcome::Enqueued
        );
    }

    #[sqlx::test]
    async fn changed_outbox_clears_automatic_terminal_cooldown(pool: PgPool) {
        let (_, remote) = actors(&pool).await;
        let old = "https://remote.example/old-outbox";
        set_actor_metadata(&pool, remote, Some(old), None)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE remote_history_states SET state = 'unsupported',
                 automatic_retry_at = now() + interval '7 days',
                 last_error_class = 'empty_collection'
             WHERE account_id = $1",
            remote,
        )
        .execute(&pool)
        .await
        .unwrap();

        set_actor_metadata(&pool, remote, Some(old), None)
            .await
            .unwrap();
        assert_eq!(
            snapshot(&pool, remote).await.unwrap().unwrap().state,
            "unsupported"
        );

        set_actor_metadata(
            &pool,
            remote,
            Some("https://remote.example/new-outbox"),
            None,
        )
        .await
        .unwrap();
        let reset = snapshot(&pool, remote).await.unwrap().unwrap();
        assert_eq!(reset.state, "idle");
        assert!(reset.automatic_retry_at.is_none());
        assert!(reset.last_error_class.is_none());
    }

    #[sqlx::test]
    async fn one_hundred_concurrent_opens_coalesce_before_charging(pool: PgPool) {
        let (local, remote) = actors(&pool).await;
        set_actor_metadata(&pool, remote, Some("https://remote.example/outbox"), None)
            .await
            .unwrap();
        save_settings(&pool, true, 90, false).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let pool = pool.clone();
            tasks.spawn(async move {
                enqueue(
                    &pool,
                    remote,
                    JobKind::Initial,
                    None,
                    "https://remote.example",
                    Some(local),
                    Admission::Explicit,
                )
                .await
                .unwrap()
            });
        }
        let mut enqueued = 0;
        let mut coalesced = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                EnqueueOutcome::Enqueued => enqueued += 1,
                EnqueueOutcome::Coalesced => coalesced += 1,
                other => panic!("unexpected enqueue result: {other:?}"),
            }
        }
        assert_eq!((enqueued, coalesced), (1, 99));
        assert_eq!(pending_count(&pool).await.unwrap(), 1);
        let charged = sqlx::query_scalar!(
            "SELECT count FROM rate_limit_windows
             WHERE bucket = 'remote_history' AND identity = $1",
            local.to_string(),
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(charged, 1);
    }

    #[sqlx::test]
    async fn local_user_intent_budget_is_thirty_per_hour(pool: PgPool) {
        let (local, _) = actors(&pool).await;
        save_settings(&pool, true, 90, false).await.unwrap();
        for n in 0..31 {
            let domain = format!("origin-{n}.example");
            let username = format!("user-{n}");
            let remote = remote_named(&pool, &username, &domain).await;
            set_actor_metadata(
                &pool,
                remote,
                Some(&format!("https://{domain}/outbox")),
                None,
            )
            .await
            .unwrap();
            let outcome = enqueue(
                &pool,
                remote,
                JobKind::Initial,
                None,
                &format!("https://{domain}"),
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap();
            if n < 30 {
                assert_eq!(outcome, EnqueueOutcome::Enqueued);
                sqlx::query!(
                    "DELETE FROM remote_history_jobs WHERE account_id = $1",
                    remote
                )
                .execute(&pool)
                .await
                .unwrap();
            } else {
                assert!(matches!(outcome, EnqueueOutcome::RateLimited(_)));
            }
        }
    }

    #[sqlx::test]
    async fn claims_are_fair_and_origin_queue_depth_is_bounded(pool: PgPool) {
        let (local, bob) = actors(&pool).await;
        let carol = remote_named(&pool, "carol", "remote.example").await;
        let dave = remote_named(&pool, "dave", "other.example").await;
        for (id, domain) in [
            (bob, "remote.example"),
            (carol, "remote.example"),
            (dave, "other.example"),
        ] {
            set_actor_metadata(&pool, id, Some(&format!("https://{domain}/outbox")), None)
                .await
                .unwrap();
        }
        save_settings(&pool, true, 90, false).await.unwrap();
        for (id, origin) in [
            (bob, "https://remote.example"),
            (carol, "https://remote.example"),
            (dave, "https://other.example"),
        ] {
            assert_eq!(
                enqueue(
                    &pool,
                    id,
                    JobKind::Initial,
                    None,
                    origin,
                    Some(local),
                    Admission::Explicit,
                )
                .await
                .unwrap(),
                EnqueueOutcome::Enqueued
            );
        }
        let erin = remote_named(&pool, "erin", "remote.example").await;
        set_actor_metadata(
            &pool,
            erin,
            Some("https://remote.example/outbox/erin"),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            enqueue(
                &pool,
                erin,
                JobKind::Initial,
                None,
                "https://remote.example",
                Some(local),
                Admission::Explicit,
            )
            .await
            .unwrap(),
            EnqueueOutcome::OriginBusy(_)
        ));

        let claimed = claim_due(&pool, "fair-worker", 4).await.unwrap();
        assert_eq!(claimed.len(), 2, "one due job from each origin");
        let remote_claims = claimed
            .iter()
            .filter(|job| job.origin == "https://remote.example")
            .count();
        assert_eq!(remote_claims, 1);
    }

    #[sqlx::test]
    async fn pruning_is_bounded_and_preserves_local_interactions_and_threads(pool: PgPool) {
        let (local, remote) = actors(&pool).await;
        let evict_first = cold_status(&pool, remote, "evict-first").await;
        let evict_second = cold_status(&pool, remote, "evict-second").await;
        let bookmarked = cold_status(&pool, remote, "bookmarked").await;
        let thread_root = cold_status(&pool, remote, "thread-root").await;
        crate::bookmark::create(&pool, local, bookmarked.id)
            .await
            .unwrap();
        status::create_local(
            &pool,
            NewLocalStatus::new(local, "local reply", "public", Some(thread_root.id)),
        )
        .await
        .unwrap();
        for (row, age) in [
            (evict_first.id, 150_i32),
            (evict_second.id, 140),
            (bookmarked.id, 130),
            (thread_root.id, 120),
        ] {
            sqlx::query!(
                "UPDATE statuses SET history_fetched_at = now() - make_interval(days => $2),
                                     history_last_touched_at = now() - make_interval(days => $2)
                 WHERE id = $1",
                row,
                age,
            )
            .execute(&pool)
            .await
            .unwrap();
        }

        assert_eq!(prune(&pool, 90, 1).await.unwrap(), 1);
        assert!(
            status::find_by_id(&pool, evict_first.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            status::find_by_id(&pool, evict_second.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(prune(&pool, 90, 10).await.unwrap(), 1);
        assert!(
            status::find_by_id(&pool, evict_second.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            status::find_by_id(&pool, bookmarked.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            status::find_by_id(&pool, thread_root.id)
                .await
                .unwrap()
                .is_some()
        );
    }
}
