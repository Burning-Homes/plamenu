//! Selection and enqueue share a transaction; receipts reserve each channel
//! across workers. No SMTP configuration means no queue or clock advancement.
use crate::{AppState, error::ApiError};
use plamenu_db::{DbError, email};
pub async fn enqueue_due(state: &AppState) -> Result<u64, ApiError> {
    if !crate::mailer::enabled(state) {
        return Ok(0);
    }
    let mut tx = state.pool.begin().await.map_err(DbError::from)?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(176,1)")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if !locked {
        return Ok(0);
    }
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_mail_batches")
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
    if pending >= 2000 {
        return Ok(0);
    }
    let recipients:Vec<(i64,String)>=sqlx::query_as(r"
        SELECT n.account_id,c.channel FROM notification_delivery_preferences p
        JOIN users u ON u.account_id=p.account_id JOIN accounts a ON a.id=p.account_id
        JOIN notifications n ON n.account_id=p.account_id AND p.kind=ANY(n.reasons)
        CROSS JOIN (VALUES ('email'),('email_digest')) c(channel)
        LEFT JOIN notification_digest_clock d ON d.account_id=n.account_id
        WHERE a.deleted_at IS NULL AND a.suspended_at IS NULL
          AND u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled
          AND u.email IS NOT NULL
          AND NOT n.filtered AND NOT sender_filtered(n.account_id,n.from_account_id)
          AND CASE WHEN c.channel='email' THEN p.email AND n.created_at>=p.email_since
              ELSE p.email_digest AND CASE WHEN d.next_digest_at IS NULL THEN EXTRACT(hour FROM now() AT TIME ZONE 'UTC') >= $1::int ELSE d.next_digest_at<=now() END END
          AND NOT EXISTS(SELECT 1 FROM notification_mail_batches b WHERE b.account_id=n.account_id AND b.channel=c.channel)
          AND NOT EXISTS(SELECT 1 FROM notification_mail_receipts r WHERE r.notification_id=n.id AND r.channel=c.channel AND (r.sent_at IS NOT NULL OR r.email_job_id IS NOT NULL OR r.retry_at>now()))
        GROUP BY n.account_id,c.channel ORDER BY min(n.id) LIMIT 10
    ").bind(i32::from(state.config.notifications.digest_hour)).fetch_all(&mut *tx).await.map_err(DbError::from)?;
    let mut queued = 0;
    for (account, channel) in recipients {
        let ids:Vec<i64>=sqlx::query_scalar(r"
            SELECT n.id FROM notifications n WHERE n.account_id=$1 AND NOT n.filtered
              AND NOT sender_filtered($1,n.from_account_id)
              AND EXISTS(SELECT 1 FROM notification_delivery_preferences p WHERE p.account_id=$1 AND p.kind=ANY(n.reasons) AND CASE WHEN $2='email' THEN p.email AND n.created_at>=p.email_since ELSE p.email_digest END)
              AND NOT EXISTS(SELECT 1 FROM notification_mail_receipts r WHERE r.notification_id=n.id AND r.channel=$2 AND (r.sent_at IS NOT NULL OR r.email_job_id IS NOT NULL OR r.retry_at>now()))
            ORDER BY n.id LIMIT 500
        ").bind(account).bind(&channel).fetch_all(&mut *tx).await.map_err(DbError::from)?;
        if ids.is_empty() {
            continue;
        }
        // Envelope is rebuilt at send time from the current verified address.
        let job = email::enqueue_tx(
            &mut tx,
            &email::OutgoingEmail {
                recipient: "pending@localhost",
                subject: "Pending notification summary",
                body: "Pending current-recipient verification",
            },
        )
        .await?;
        sqlx::query("INSERT INTO notification_mail_batches(email_job_id,account_id,channel) VALUES($1,$2,$3)").bind(job).bind(account).bind(&channel).execute(&mut *tx).await.map_err(DbError::from)?;
        sqlx::query("INSERT INTO notification_mail_receipts(notification_id,channel,email_job_id) SELECT id,$2,$3 FROM unnest($1::bigint[]) id ON CONFLICT(notification_id,channel) DO UPDATE SET email_job_id=EXCLUDED.email_job_id")
            .bind(ids).bind(channel).bind(job).execute(&mut *tx).await.map_err(DbError::from)?;
        queued += 1;
    }
    tx.commit().await.map_err(DbError::from)?;
    Ok(queued)
}
