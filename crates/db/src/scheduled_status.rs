//! Scheduled posts remain queued until publication commits. Claims are leased;
//! their generations fence stale workers and rescheduled entries.

use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduledStatus {
    pub id: i64,
    pub account_id: i64,
    pub scheduled_at: OffsetDateTime,
    pub text: String,
    pub object_type: String,
    pub video_channel_id: Option<i64>,
    pub title: Option<String>,
    /// The format `text` was authored in (P4, Pleroma's `content_type`),
    /// captured at schedule time and replayed on publish.
    pub content_type: String,
    pub visibility: String,
    pub in_reply_to_id: Option<i64>,
    pub quoted_status_id: Option<i64>,
    pub spoiler_text: String,
    pub sensitive: bool,
    pub language: Option<String>,
    pub application_id: Option<i64>,
    pub media_ids: Vec<i64>,
    pub poll_options: Option<Vec<String>>,
    pub poll_expires_in: Option<i64>,
    pub poll_multiple: bool,
    pub poll_hide_totals: bool,
    /// The quote-approval bitmap resolved when the status was scheduled
    /// (Mastodon captures it in the scheduled params); `None` on rows queued
    /// before the column existed.
    pub quote_approval_policy: Option<i32>,
    pub created_at: OffsetDateTime,
    /// Changes whenever a worker claims this row or the member reschedules it.
    pub publish_generation: i64,
}

const COLS: &str = "id, account_id, scheduled_at, text, content_type, visibility, \
                    in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language, \
                    application_id, media_ids, poll_options, poll_expires_in, \
                    poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation";
const _: &str = COLS; // documentation: every query selects exactly these

#[derive(Debug)]
pub struct NewScheduledStatus<'a> {
    pub account_id: i64,
    pub scheduled_at: OffsetDateTime,
    pub text: &'a str,
    pub object_type: &'a str,
    pub video_channel_id: Option<i64>,
    pub title: Option<&'a str>,
    pub content_type: &'a str,
    pub visibility: &'a str,
    pub in_reply_to_id: Option<i64>,
    pub quoted_status_id: Option<i64>,
    pub spoiler_text: &'a str,
    pub sensitive: bool,
    pub language: Option<&'a str>,
    pub application_id: Option<i64>,
    pub media_ids: &'a [i64],
    pub poll_options: Option<&'a [String]>,
    pub poll_expires_in: Option<i64>,
    pub poll_multiple: bool,
    pub poll_hide_totals: bool,
    pub quote_approval_policy: Option<i32>,
}

pub async fn create<'e, E: PgExecutor<'e>>(
    executor: E,
    new: NewScheduledStatus<'_>,
) -> Result<ScheduledStatus, DbError> {
    let row = sqlx::query_as!(
        ScheduledStatus,
        r#"
        INSERT INTO scheduled_statuses
            (id, account_id, scheduled_at, text, content_type, visibility,
             in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
             application_id, media_ids, poll_options, poll_expires_in, poll_multiple,
             poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                $18, $19, $20, $21)
        RETURNING id, account_id, scheduled_at, text, content_type, visibility,
                  in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
                  application_id, media_ids, poll_options, poll_expires_in,
                  poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
        "#,
        id::next(),
        new.account_id,
        new.scheduled_at,
        new.text,
        new.content_type,
        new.visibility,
        new.in_reply_to_id,
        new.quoted_status_id,
        new.spoiler_text,
        new.sensitive,
        new.language,
        new.application_id,
        new.media_ids,
        new.poll_options,
        new.poll_expires_in,
        new.poll_multiple,
        new.poll_hide_totals,
        new.quote_approval_policy,
        new.object_type,
        new.title,
        new.video_channel_id,
    )
    .fetch_one(executor)
    .await?;
    Ok(row)
}

/// One scheduled status owned by `account_id`.
pub async fn find_for_account(
    pool: &PgPool,
    account_id: i64,
    id: i64,
) -> Result<Option<ScheduledStatus>, DbError> {
    let row = sqlx::query_as!(
        ScheduledStatus,
        r#"
        SELECT id, account_id, scheduled_at, text, content_type, visibility,
               in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
               application_id, media_ids, poll_options, poll_expires_in,
               poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
        FROM scheduled_statuses
        WHERE id = $1 AND account_id = $2
        "#,
        id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// One account's scheduled statuses, keyset-paginated by id (newest first).
pub async fn list_for_account(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<ScheduledStatus>, DbError> {
    if let Some(min_id) = min_id {
        let mut rows = sqlx::query_as!(
            ScheduledStatus,
            r#"
            SELECT id, account_id, scheduled_at, text, content_type, visibility,
                   in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
                   application_id, media_ids, poll_options, poll_expires_in,
                   poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
            FROM scheduled_statuses
            WHERE account_id = $1 AND id > $2
              AND ($3::bigint IS NULL OR id < $3)
            ORDER BY id ASC
            LIMIT $4
            "#,
            account_id,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        ScheduledStatus,
        r#"
        SELECT id, account_id, scheduled_at, text, content_type, visibility,
               in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
               application_id, media_ids, poll_options, poll_expires_in,
               poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
        FROM scheduled_statuses
        WHERE account_id = $1
          AND ($2::bigint IS NULL OR id < $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Updates only `scheduled_at` (the one field Mastodon's PATCH permits).
pub async fn update_scheduled_at(
    pool: &PgPool,
    account_id: i64,
    id: i64,
    scheduled_at: OffsetDateTime,
) -> Result<Option<ScheduledStatus>, DbError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query_as!(
        ScheduledStatus,
        r#"
        UPDATE scheduled_statuses
        SET scheduled_at = $3, publish_after = NULL, publish_generation = publish_generation + 1
        WHERE id = $1 AND account_id = $2
        RETURNING id, account_id, scheduled_at, text, content_type, visibility,
                  in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
                  application_id, media_ids, poll_options, poll_expires_in,
                  poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
        "#,
        id,
        account_id,
        scheduled_at,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if row.as_ref().and_then(|row| row.video_channel_id).is_some() {
        sqlx::query(
            "UPDATE video_drafts d SET expires_at = GREATEST(now() + interval '7 days', $2 + interval '7 days') \
             FROM media_attachments m WHERE m.id = d.media_id AND m.scheduled_status_id = $1",
        )
        .bind(id)
        .bind(scheduled_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(row)
}

/// Deletes one owned scheduled status; the `ON DELETE SET NULL` on
/// `media_attachments.scheduled_status_id` releases any reserved media.
pub async fn delete(pool: &PgPool, account_id: i64, id: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    // Keep a cancelled Video draft available for another seven days instead
    // of retaining its source until the originally scheduled date.
    sqlx::query(
        "UPDATE video_drafts d SET expires_at = now() + interval '7 days' \
         FROM media_attachments m JOIN scheduled_statuses s ON s.id = m.scheduled_status_id \
         WHERE m.id = d.media_id AND s.id = $1 AND s.account_id = $2 \
           AND s.video_channel_id IS NOT NULL",
    )
    .bind(id)
    .bind(account_id)
    .execute(&mut *tx)
    .await?;
    let result = sqlx::query!(
        "DELETE FROM scheduled_statuses WHERE id = $1 AND account_id = $2",
        id,
        account_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Total scheduled statuses for an account (Mastodon's 300-row cap).
pub async fn count_total(pool: &PgPool, account_id: i64) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM scheduled_statuses WHERE account_id = $1",
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}

/// Scheduled statuses an account already has on the same **owner-local** date
/// as `scheduled_at` (Mastodon's 25-per-day cap).
///
/// Bucketed in `zone`, not UTC, because this is a *user quota* rather than an
/// instance metric: "you can schedule N posts per day" has to mean the day the
/// owner is living in, or someone at UTC+12 hits their cap halfway through
/// their afternoon. (Instance analytics stay UTC-bucketed for the opposite
/// reason — see `crate::metrics`.) This is a deliberate divergence from
/// Mastodon, which buckets the cap in UTC for everyone.
///
/// `zone` is interpolated into `AT TIME ZONE` and must come from the server's
/// zone inventory, never from user input.
pub async fn count_on_day(
    pool: &PgPool,
    account_id: i64,
    scheduled_at: OffsetDateTime,
    zone: &str,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        "SELECT count(*) FROM scheduled_statuses
         WHERE account_id = $1
           AND (scheduled_at AT TIME ZONE $3)::date
               = ($2::timestamptz AT TIME ZONE $3)::date",
        account_id,
        scheduled_at,
        zone,
    )
    .fetch_one(pool)
    .await?;
    Ok(count.unwrap_or(0))
}

/// Claims due rows for five minutes without removing them. A failed or crashed
/// attempt becomes eligible again when `publish_after` passes. The generation
/// must be checked inside the transaction that creates the post.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<ScheduledStatus>, DbError> {
    let rows = sqlx::query_as!(
        ScheduledStatus,
        r#"
        UPDATE scheduled_statuses
        SET publish_after = now() + interval '5 minutes',
            publish_generation = publish_generation + 1
        WHERE id IN (
            SELECT id FROM scheduled_statuses
            WHERE GREATEST(scheduled_at, publish_after) <= now()
            ORDER BY GREATEST(scheduled_at, publish_after), id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, account_id, scheduled_at, text, content_type, visibility,
                  in_reply_to_id, quoted_status_id, spoiler_text, sensitive, language,
                  application_id, media_ids, poll_options, poll_expires_in,
                  poll_multiple, poll_hide_totals, quote_approval_policy, object_type, title, video_channel_id, created_at, publish_generation
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Consume only the current claim, on the transaction that publishes its post.
/// Rollback restores the queue entry and its media reservations. A concurrent
/// publisher, cancellation, or reschedule makes a stale claim return `false`.
pub async fn consume_claim(
    conn: &mut sqlx::PgConnection,
    id: i64,
    account_id: i64,
    generation: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM scheduled_statuses
         WHERE id = $1 AND account_id = $2 AND publish_generation = $3
           AND publish_generation > 0",
        id,
        account_id,
        generation,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}
