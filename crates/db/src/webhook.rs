//! Admin-configured webhooks — Mastodon's `Webhook` model — plus their
//! delivery queue. A webhook stores an HTTPS callback URL, a random shared
//! secret, the enabled event names, and an optional payload template.
//! Triggers serialize the event payload once and enqueue one
//! `webhook_delivery_jobs` row per matching webhook (Mastodon's
//! `WebhookService`); the worker claims, signs and POSTs them.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::role::permission;
use crate::{DbError, id};

/// How long a claimed webhook job stays invisible before a crashed worker's
/// batch becomes due again.
const LEASE_SECONDS: f64 = 300.0;

/// Give up on a delivery after this many attempts (~16h of backoff — the
/// federation queue's bound; Mastodon's sidekiq `retry: 16` waits days).
pub const MAX_DELIVERY_ATTEMPTS: i32 = 8;

pub const ACCOUNT_APPROVED: &str = "account.approved";
pub const ACCOUNT_CREATED: &str = "account.created";
pub const ACCOUNT_UPDATED: &str = "account.updated";
pub const REPORT_CREATED: &str = "report.created";
pub const REPORT_UPDATED: &str = "report.updated";
pub const STATUS_CREATED: &str = "status.created";
pub const STATUS_UPDATED: &str = "status.updated";

/// Event names accepted by Mastodon 4.6 webhooks.
pub const EVENTS: &[&str] = &[
    ACCOUNT_APPROVED,
    ACCOUNT_CREATED,
    ACCOUNT_UPDATED,
    REPORT_CREATED,
    REPORT_UPDATED,
    STATUS_CREATED,
    STATUS_UPDATED,
];

#[derive(Clone, sqlx::FromRow)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub events: Vec<String>,
    pub secret: String,
    pub template: Option<String>,
    pub enabled: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub struct NewWebhook<'a> {
    pub url: &'a str,
    pub events: &'a [String],
    pub secret: &'a str,
    pub template: Option<&'a str>,
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Webhook")
            .field("id", &self.id)
            .field("url", &"[REDACTED]")
            .field("events", &self.events)
            .field("secret", &"[REDACTED]")
            .field("template_configured", &self.template.is_some())
            .field("enabled", &self.enabled)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl std::fmt::Debug for NewWebhook<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NewWebhook")
            .field("url", &"[REDACTED]")
            .field("events", &self.events)
            .field("secret", &"[REDACTED]")
            .field("template_configured", &self.template.is_some())
            .finish()
    }
}

#[derive(Debug)]
pub struct WebhookUpdate<'a> {
    pub url: &'a str,
    pub events: &'a [String],
    pub template: Option<&'a str>,
}

#[must_use]
pub fn event_label(event: &str) -> &'static str {
    match event {
        ACCOUNT_APPROVED => "Account approved",
        ACCOUNT_CREATED => "Account created",
        ACCOUNT_UPDATED => "Account updated",
        REPORT_CREATED => "Report created",
        REPORT_UPDATED => "Report updated",
        STATUS_CREATED => "Status created",
        STATUS_UPDATED => "Status updated",
        _ => "Unknown event",
    }
}

/// Mastodon's `Webhook.permission_for_event`.
#[must_use]
pub fn permission_for_event(event: &str) -> Option<i64> {
    match event {
        ACCOUNT_APPROVED | ACCOUNT_CREATED | ACCOUNT_UPDATED => Some(permission::MANAGE_USERS),
        REPORT_CREATED | REPORT_UPDATED => Some(permission::MANAGE_REPORTS),
        STATUS_CREATED | STATUS_UPDATED => Some(permission::VIEW_DEVOPS),
        _ => None,
    }
}

/// The distinct role permissions needed to create/update/delete this webhook.
#[must_use]
pub fn required_permissions(events: &[String]) -> Vec<i64> {
    let mut required = Vec::new();
    for event in events {
        let Some(permission) = permission_for_event(event) else {
            continue;
        };
        if !required.contains(&permission) {
            required.push(permission);
        }
    }
    required
}

pub async fn list(pool: &PgPool) -> Result<Vec<Webhook>, DbError> {
    let rows = sqlx::query_as!(
        Webhook,
        r#"
        SELECT id, url, events, secret, template, enabled, created_at, updated_at
        FROM webhooks
        ORDER BY id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn find_by_id(pool: &PgPool, webhook_id: i64) -> Result<Option<Webhook>, DbError> {
    let row = sqlx::query_as!(
        Webhook,
        r#"
        SELECT id, url, events, secret, template, enabled, created_at, updated_at
        FROM webhooks
        WHERE id = $1
        "#,
        webhook_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create(pool: &PgPool, data: NewWebhook<'_>) -> Result<Webhook, DbError> {
    let row = sqlx::query_as!(
        Webhook,
        r#"
        INSERT INTO webhooks (id, url, events, secret, template)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, url, events, secret, template, enabled, created_at, updated_at
        "#,
        id::next(),
        data.url,
        data.events,
        data.secret,
        data.template,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update(
    pool: &PgPool,
    webhook_id: i64,
    data: WebhookUpdate<'_>,
) -> Result<Option<Webhook>, DbError> {
    let row = sqlx::query_as!(
        Webhook,
        r#"
        UPDATE webhooks
        SET url = $2,
            events = $3,
            template = $4,
            updated_at = now()
        WHERE id = $1
        RETURNING id, url, events, secret, template, enabled, created_at, updated_at
        "#,
        webhook_id,
        data.url,
        data.events,
        data.template,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn set_enabled(
    pool: &PgPool,
    webhook_id: i64,
    enabled: bool,
) -> Result<Option<Webhook>, DbError> {
    let row = sqlx::query_as!(
        Webhook,
        r#"
        UPDATE webhooks
        SET enabled = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, url, events, secret, template, enabled, created_at, updated_at
        "#,
        webhook_id,
        enabled,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn rotate_secret(
    pool: &PgPool,
    webhook_id: i64,
    secret: &str,
) -> Result<Option<Webhook>, DbError> {
    let row = sqlx::query_as!(
        Webhook,
        r#"
        UPDATE webhooks
        SET secret = $2, updated_at = now()
        WHERE id = $1
        RETURNING id, url, events, secret, template, enabled, created_at, updated_at
        "#,
        webhook_id,
        secret,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete(pool: &PgPool, webhook_id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!("DELETE FROM webhooks WHERE id = $1", webhook_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// The enabled webhooks subscribed to `event` — Mastodon's
/// `Webhook.enabled.where('? = ANY(events)', event)`.
pub async fn enabled_for_event(pool: &PgPool, event: &str) -> Result<Vec<Webhook>, DbError> {
    let rows = sqlx::query_as!(
        Webhook,
        r#"
        SELECT id, url, events, secret, template, enabled, created_at, updated_at
        FROM webhooks
        WHERE enabled AND $1 = ANY(events)
        ORDER BY id
        "#,
        event,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryJob {
    pub id: i64,
    pub webhook_id: i64,
    pub event: String,
    /// The full payload, serialized at trigger time.
    pub body: String,
    /// Attempts so far, including the one being made now.
    pub attempts: i32,
}

/// Queues one delivery of an already-serialized payload to a webhook.
pub async fn enqueue_delivery(
    pool: &PgPool,
    webhook_id: i64,
    event: &str,
    body: &str,
) -> Result<i64, DbError> {
    let job_id = sqlx::query_scalar!(
        r#"
        INSERT INTO webhook_delivery_jobs (webhook_id, event, body)
        VALUES ($1, $2, $3)
        RETURNING id
        "#,
        webhook_id,
        event,
        body,
    )
    .fetch_one(pool)
    .await?;
    Ok(job_id)
}

/// Claims up to `limit` due webhook jobs, bumping their attempt counter and
/// leasing them for [`LEASE_SECONDS`].
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<DeliveryJob>, DbError> {
    let jobs = sqlx::query_as!(
        DeliveryJob,
        r#"
        UPDATE webhook_delivery_jobs SET
            attempts = attempts + 1,
            run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT id FROM webhook_delivery_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, webhook_id, event, body, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a delivered (or permanently failed) webhook job.
pub async fn complete_delivery(pool: &PgPool, job_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM webhook_delivery_jobs WHERE id = $1", job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reschedules a failed delivery with exponential backoff (60s, 4m, 16m, ~1h).
pub async fn retry_delivery_later(
    pool: &PgPool,
    job_id: i64,
    attempts: i32,
) -> Result<(), DbError> {
    let exp = u32::try_from(attempts.clamp(1, 9)).unwrap_or(1) - 1;
    let delay = f64::from((60u32 * 4u32.saturating_pow(exp)).min(21_600));
    sqlx::query!(
        "UPDATE webhook_delivery_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        job_id,
        delay,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Number of deliveries not yet made (diagnostics / tests).
pub async fn pending_deliveries(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM webhook_delivery_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Makes every queued delivery due right now (tests).
pub async fn make_all_deliveries_due(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!("UPDATE webhook_delivery_jobs SET run_at = now()")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn delivery_queue_lifecycle(pool: PgPool) {
        let hook = create(
            &pool,
            NewWebhook {
                url: "https://example.com/hook",
                events: &[REPORT_CREATED.to_owned()],
                secret: "s3cret",
                template: None,
            },
        )
        .await
        .unwrap();

        let matching = enabled_for_event(&pool, REPORT_CREATED).await.unwrap();
        assert_eq!(matching.len(), 1);
        assert!(
            enabled_for_event(&pool, STATUS_CREATED)
                .await
                .unwrap()
                .is_empty()
        );
        set_enabled(&pool, hook.id, false).await.unwrap();
        assert!(
            enabled_for_event(&pool, REPORT_CREATED)
                .await
                .unwrap()
                .is_empty()
        );

        enqueue_delivery(
            &pool,
            hook.id,
            REPORT_CREATED,
            r#"{"event":"report.created"}"#,
        )
        .await
        .unwrap();
        let jobs = claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].webhook_id, hook.id);
        assert_eq!(jobs[0].attempts, 1);
        // Leased: nothing else due.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        retry_delivery_later(&pool, jobs[0].id, jobs[0].attempts)
            .await
            .unwrap();
        make_all_deliveries_due(&pool).await.unwrap();
        let again = claim_due(&pool, 10).await.unwrap();
        assert_eq!(again[0].attempts, 2);

        // Deleting the webhook cascades its jobs away.
        delete(&pool, hook.id).await.unwrap();
        assert_eq!(pending_deliveries(&pool).await.unwrap(), 0);
    }

    #[test]
    fn event_permissions_match_mastodon() {
        assert_eq!(
            permission_for_event(ACCOUNT_CREATED),
            Some(permission::MANAGE_USERS)
        );
        assert_eq!(
            permission_for_event(REPORT_UPDATED),
            Some(permission::MANAGE_REPORTS)
        );
        assert_eq!(
            permission_for_event(STATUS_CREATED),
            Some(permission::VIEW_DEVOPS)
        );
        assert_eq!(permission_for_event("nope"), None);
    }

    #[test]
    fn required_permissions_are_distinct() {
        let events = vec![
            ACCOUNT_CREATED.to_owned(),
            ACCOUNT_UPDATED.to_owned(),
            REPORT_CREATED.to_owned(),
        ];
        assert_eq!(
            required_permissions(&events),
            vec![permission::MANAGE_USERS, permission::MANAGE_REPORTS]
        );
    }
}
