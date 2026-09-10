//! The outgoing e-mail queue (M21) — the DB-backed analogue of Mastodon's
//! sidekiq `mailers` queue. Messages are rendered at enqueue time (recipient,
//! subject, plain-text body); the server's mailer worker claims due rows,
//! hands them to SMTP and retries transient failures with the delivery-queue
//! backoff.

use sqlx::PgPool;

use crate::DbError;

/// How long a claimed e-mail job stays invisible before a crashed worker's
/// batch becomes due again.
const LEASE_SECONDS: f64 = 300.0;

/// Give up on a message after this many attempts (~16h of backoff, matching
/// the webhook delivery queue's bound).
pub const MAX_SEND_ATTEMPTS: i32 = 8;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EmailJob {
    pub id: i64,
    pub recipient: String,
    pub subject: String,
    /// The plain-text message body, rendered at enqueue time.
    pub body: String,
    /// Attempts so far, including the one being made now.
    pub attempts: i32,
}

/// A fully-rendered message, ready to enqueue. The credential-transition flows
/// render this *before* touching the database, then enqueue it in the same
/// transaction as their token update via [`enqueue_tx`],
/// so a failed enqueue rolls back the token change and the previous
/// reset/confirmation link stays valid.
#[derive(Debug, Clone, Copy)]
pub struct OutgoingEmail<'a> {
    pub recipient: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
}

/// Queues one already-rendered message.
pub async fn enqueue(
    pool: &PgPool,
    recipient: &str,
    subject: &str,
    body: &str,
) -> Result<i64, DbError> {
    insert(pool, recipient, subject, body).await
}

/// Queues one already-rendered message inside an open transaction, so the row
/// commits or rolls back atomically with the caller's other writes — the
/// durable-enqueue half of the credential-transition fix.
pub async fn enqueue_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mail: &OutgoingEmail<'_>,
) -> Result<i64, DbError> {
    insert(&mut **tx, mail.recipient, mail.subject, mail.body).await
}

/// The shared INSERT, usable against a pool or an open transaction.
async fn insert<'e, E>(
    executor: E,
    recipient: &str,
    subject: &str,
    body: &str,
) -> Result<i64, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let job_id = sqlx::query_scalar!(
        r#"
        INSERT INTO email_jobs (recipient, subject, body)
        VALUES ($1, $2, $3)
        RETURNING id
        "#,
        recipient,
        subject,
        body,
    )
    .fetch_one(executor)
    .await?;
    Ok(job_id)
}

/// Claims up to `limit` due e-mail jobs, bumping their attempt counter and
/// leasing them for [`LEASE_SECONDS`].
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<EmailJob>, DbError> {
    let jobs = sqlx::query_as!(
        EmailJob,
        r#"
        UPDATE email_jobs SET
            attempts = attempts + 1,
            run_at = now() + make_interval(secs => $2)
        WHERE id IN (
            SELECT id FROM email_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, recipient, subject, body, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a sent (or permanently failed) e-mail job.
pub async fn complete(pool: &PgPool, job_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM email_jobs WHERE id = $1", job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reschedules a failed send with exponential backoff (60s, 4m, 16m, ~1h).
pub async fn retry_later(pool: &PgPool, job_id: i64, attempts: i32) -> Result<(), DbError> {
    let exp = u32::try_from(attempts.clamp(1, 9)).unwrap_or(1) - 1;
    let delay = f64::from((60u32 * 4u32.saturating_pow(exp)).min(21_600));
    sqlx::query!(
        "UPDATE email_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        job_id,
        delay,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Number of messages not yet sent (diagnostics / tests).
pub async fn pending(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM email_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Makes every queued message due right now (tests).
pub async fn make_all_due(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!("UPDATE email_jobs SET run_at = now()")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn queue_lifecycle(pool: PgPool) {
        enqueue(&pool, "vesna@example.com", "Confirm", "Hello Vesna")
            .await
            .unwrap();
        let jobs = claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].recipient, "vesna@example.com");
        assert_eq!(jobs[0].subject, "Confirm");
        assert_eq!(jobs[0].body, "Hello Vesna");
        assert_eq!(jobs[0].attempts, 1);
        // Leased: nothing else due.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        retry_later(&pool, jobs[0].id, jobs[0].attempts)
            .await
            .unwrap();
        make_all_due(&pool).await.unwrap();
        let again = claim_due(&pool, 10).await.unwrap();
        assert_eq!(again[0].attempts, 2);

        complete(&pool, again[0].id).await.unwrap();
        assert_eq!(pending(&pool).await.unwrap(), 0);
    }
}
