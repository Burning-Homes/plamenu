//! Durable opt-in notification mail reuses the native SMTP queue. Every send
//! rebuilds a bounded summary against current recipient/channel/sender state.
mod queue;
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, email::EmailJob};
pub use queue::enqueue_due;
use serde::Deserialize;
use std::fmt::Write;
use time::OffsetDateTime;
#[derive(sqlx::FromRow)]
struct Batch {
    account_id: i64,
    channel: String,
}
#[derive(sqlx::FromRow)]
struct Recipient {
    id: i64,
    email: String,
}
#[derive(sqlx::FromRow)]
struct Item {
    id: i64,
    kind: String,
    created_at: OffsetDateTime,
    status_id: Option<i64>,
}
pub struct Prepared {
    pub job: EmailJob,
    pub notification_ids: Vec<i64>,
}
/// None means this is a security email: its existing envelope stays intact.
/// Some(None) means a managed notification batch has no current deliverable rows.
pub async fn prepare(
    state: &AppState,
    job: &EmailJob,
) -> Result<Option<Option<Prepared>>, ApiError> {
    let batch: Option<Batch> = sqlx::query_as(
        "SELECT account_id,channel FROM notification_mail_batches WHERE email_job_id=$1",
    )
    .bind(job.id)
    .fetch_optional(&state.pool)
    .await
    .map_err(DbError::from)?;
    let Some(batch) = batch else {
        // A claimed placeholder can outlive concurrent account erasure. It
        // must never fall through to the ordinary security-mail path.
        return Ok(
            if job.recipient == "pending@localhost"
                && job.body == "Pending current-recipient verification"
            {
                Some(None)
            } else {
                None
            },
        );
    };
    // Renew immediately before work and fence a stale claimant. Earlier rows
    // in the shared queue may have consumed most of the original lease.
    let current = sqlx::query(
        "UPDATE email_jobs SET run_at=now()+interval '5 minutes' WHERE id=$1 AND attempts=$2",
    )
    .bind(job.id)
    .bind(job.attempts)
    .execute(&state.pool)
    .await
    .map_err(DbError::from)?
    .rows_affected();
    if current == 0 {
        return Ok(Some(None));
    }

    let recipient:Option<Recipient>=sqlx::query_as("SELECT u.id,u.email FROM users u JOIN accounts a ON a.id=u.account_id WHERE a.id=$1 AND a.deleted_at IS NULL AND a.suspended_at IS NULL AND u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled AND u.email IS NOT NULL")
        .bind(batch.account_id).fetch_optional(&state.pool).await.map_err(DbError::from)?;
    let Some(recipient) = recipient else {
        return Ok(Some(None));
    };
    let items:Vec<Item>=sqlx::query_as(r"
        SELECT n.id,notification_kind_for(n.reasons,ARRAY(
            SELECT reason FROM unnest(n.reasons) reason
            JOIN notification_delivery_preferences p ON p.account_id=n.account_id AND p.kind=reason
            WHERE CASE WHEN $2='email' THEN p.email ELSE p.email_digest END
        )) AS kind,n.created_at,n.status_id
        FROM notification_mail_receipts r JOIN notifications n ON n.id=r.notification_id
        WHERE r.email_job_id=$1 AND NOT n.filtered AND NOT sender_filtered(n.account_id,n.from_account_id)
          AND EXISTS(SELECT 1 FROM notification_delivery_preferences p WHERE p.account_id=n.account_id AND p.kind=ANY(n.reasons) AND CASE WHEN $2='email' THEN p.email ELSE p.email_digest END)
          AND (n.status_id IS NULL OR EXISTS(SELECT 1 FROM statuses s WHERE s.id=n.status_id AND s.deleted_at IS NULL)) -- STUBFILTER
        ORDER BY n.id LIMIT 500
    ").bind(job.id).bind(&batch.channel).fetch_all(&state.pool).await.map_err(DbError::from)?;
    let status_ids = items
        .iter()
        .filter_map(|item| item.status_id)
        .collect::<Vec<_>>();
    let statuses = plamenu_db::status::find_by_ids(&state.pool, &status_ids).await?;
    let visible = crate::entities::filter_viewable(&state.pool, &statuses, Some(batch.account_id))
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect::<std::collections::HashSet<_>>();
    let valid = items
        .into_iter()
        .filter(|item| item.status_id.is_none_or(|id| visible.contains(&id)))
        .collect::<Vec<_>>();
    if valid.is_empty() {
        return Ok(Some(None));
    }
    let l = crate::web::i18n::Locale::for_user(&state.pool, recipient.id).await?;
    let mut body = l.text("notification-delivery-mail-intro");
    body.push_str("\n\n");
    let notification_ids = valid.iter().map(|item| item.id).collect();
    for item in valid {
        let _ = writeln!(
            body,
            "- {} ({})",
            crate::web::notification_delivery::label(l, &item.kind),
            crate::entities::rfc3339(item.created_at)?
        );
    }
    let _ = writeln!(
        body,
        "\n{}: https://{}/notifications/inbox?mail=true",
        l.text("notification-delivery-mail-open"),
        state.config.domain
    );
    Ok(Some(Some(Prepared {
        notification_ids,
        job: EmailJob {
            id: job.id,
            recipient: recipient.email,
            subject: l.text(if batch.channel == "email_digest" {
                "notification-delivery-mail-digest"
            } else {
                "notification-delivery-mail-subject"
            }),
            body,
            attempts: job.attempts,
        },
    })))
}
/// Only SMTP acceptance stamps receipts. Permanent failures remain unsent and
/// become eligible on a later day; transient retries stay in the normal queue.
pub async fn finish(
    state: &AppState,
    job: &EmailJob,
    accepted: bool,
    sent_ids: &[i64],
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT id FROM email_jobs WHERE id=$1 AND attempts=$2 FOR UPDATE")
            .bind(job.id)
            .bind(job.attempts)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
    if current.is_none() {
        return Ok(());
    }

    let batch: Option<Batch> = sqlx::query_as(
        "SELECT account_id,channel FROM notification_mail_batches WHERE email_job_id=$1 FOR UPDATE",
    )
    .bind(job.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(DbError::from)?;
    if let Some(batch) = batch {
        sqlx::query("UPDATE notification_mail_receipts SET sent_at=CASE WHEN $2 AND notification_id=ANY($3) THEN now() ELSE sent_at END,retry_at=CASE WHEN channel='email_digest' THEN (date_trunc('day',now() AT TIME ZONE 'UTC')+interval '1 day'+make_interval(hours=>$4)) AT TIME ZONE 'UTC' ELSE now()+interval '1 day' END WHERE email_job_id=$1")
            .bind(job.id).bind(accepted).bind(sent_ids).bind(i32::from(state.config.notifications.digest_hour)).execute(&mut *tx).await.map_err(DbError::from)?;
        if accepted && batch.channel == "email_digest" {
            sqlx::query("INSERT INTO notification_digest_clock(account_id,next_digest_at) VALUES($1,(date_trunc('day',now() AT TIME ZONE 'UTC')+interval '1 day'+make_interval(hours=>$2)) AT TIME ZONE 'UTC') ON CONFLICT(account_id) DO UPDATE SET next_digest_at=EXCLUDED.next_digest_at")
                .bind(batch.account_id).bind(i32::from(state.config.notifications.digest_hour)).execute(&mut *tx).await.map_err(DbError::from)?;
        }
    }
    sqlx::query("DELETE FROM email_jobs WHERE id=$1")
        .bind(job.id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from)?;
    tx.commit().await.map_err(DbError::from)?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub items: Vec<plamenu_db::notification_delivery::Preference>,
}
pub fn validate(items: &[plamenu_db::notification_delivery::Preference]) -> Result<(), ApiError> {
    let types = plamenu_db::notification_delivery::TYPES;
    let mut seen = std::collections::HashSet::new();
    if items.len() > types.len()
        || items
            .iter()
            .any(|p| !types.contains(&p.kind.as_str()) || !seen.insert(&p.kind))
    {
        return Err(ApiError::Unprocessable(
            "Unknown or duplicate notification kind".into(),
        ));
    }
    Ok(())
}
