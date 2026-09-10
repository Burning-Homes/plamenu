//! The Postgres-backed delivery queue.
//!
//! Design (deliberately Redis-free): jobs are rows; a worker claims a due
//! batch with `FOR UPDATE SKIP LOCKED` — safe under concurrent workers —
//! and the claim itself leases the job by pushing `run_at` into the future,
//! so deliveries lost to a crash simply become due again.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgListener;

use crate::{DbError, id};

/// How long a claimed job stays invisible before a crashed worker's batch
/// becomes due again.
const LEASE_SECONDS: f64 = 300.0;

/// Give up on a delivery after this many attempts (~16h of backoff).
pub const MAX_ATTEMPTS: i32 = 8;

/// How long a cancellable job stays undue, so a retraction fired right after
/// the interaction (unboost chasing its boost) can [`cancel`] it in-queue
/// instead of racing it through delivery: receivers process inbox POSTs
/// concurrently (Mastodon's Sidekiq `ingress` queue has no per-sender
/// ordering), so once both activities are on the wire an `Undo` can be
/// processed before its `Announce` and the boost sticks remotely forever.
const CANCELLATION_GRACE_SECONDS: f64 = 1.0;

/// A peer may emit a `Follow` before its local relationship transaction has
/// committed. An immediate local `Accept` can then arrive while the peer still
/// has nothing to match it to and be discarded as a harmless-looking no-op.
/// Holding only relationship responses for this short grace closes that race
/// without delaying ordinary federation fan-out.
const RELATIONSHIP_RESPONSE_GRACE_SECONDS: f64 = 1.0;

/// The NOTIFY channel that wakes delivery workers when new work arrives.
pub const CHANNEL: &str = "plamenu_delivery";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryJob {
    pub id: i64,
    /// The local account whose key signs the delivery; `None` means the
    /// instance actor signs it (Mastodon's report anonymity for `Flag`).
    pub account_id: Option<i64>,
    pub inbox_url: String,
    pub activity: Value,
    pub synchronize_followers: bool,
    /// Attempts so far, including the one being made now.
    pub attempts: i32,
}

/// Enqueues an activity for delivery, signed by `account_id`.
pub async fn enqueue<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(pool, Some(account_id), inbox_url, activity, false, 0.0).await
}

/// Enqueues an `Accept(Follow)` or `Reject(Follow)` after a short commit grace
/// for the initiating peer's relationship row.
pub async fn enqueue_relationship_response(
    pool: &PgPool,
    account_id: i64,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(
        pool,
        Some(account_id),
        inbox_url,
        activity,
        false,
        RELATIONSHIP_RESPONSE_GRACE_SECONDS,
    )
    .await
}

/// Enqueues a retractable interaction (`Like`/`Announce`/`EmojiReact`) held
/// back for [`CANCELLATION_GRACE_SECONDS`] so an immediate retraction can
/// [`cancel`] it before it ever leaves the queue.
pub async fn enqueue_cancellable<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(
        pool,
        Some(account_id),
        inbox_url,
        activity,
        false,
        CANCELLATION_GRACE_SECONDS,
    )
    .await
}

/// Enqueues an activity for delivery with Mastodon's followers
/// synchronization header enabled. Used for followers-only status
/// distribution.
pub async fn enqueue_synchronized<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(pool, Some(account_id), inbox_url, activity, true, 0.0).await
}

/// Enqueues an activity signed by the instance actor rather than a local
/// user — what `Flag` (federated reports) uses, so the reporting user stays
/// anonymous to the target's server.
pub async fn enqueue_from_instance(
    pool: &PgPool,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(pool, None, inbox_url, activity, false, 0.0).await
}

/// Batched public update from the instance actor, used to distribute a newly
/// published verification key while the old key still signs deliveries.
pub async fn enqueue_many_from_instance<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    inbox_urls: &[String],
    activity: &Value,
) -> Result<(), DbError> {
    enqueue_many_inner(pool, None, inbox_urls, activity, false, 0.0).await
}

/// Enqueues one activity for delivery to many inboxes in a single statement —
/// the fan-out path. Per-inbox [`enqueue`] calls made posting O(#inboxes)
/// database round trips (~350 ms at 1.5k shared inboxes); the batch is one.
pub async fn enqueue_many<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    inbox_urls: &[String],
    activity: &Value,
    synchronize_followers: bool,
) -> Result<(), DbError> {
    enqueue_many_inner(
        pool,
        Some(account_id),
        inbox_urls,
        activity,
        synchronize_followers,
        0.0,
    )
    .await
}

/// Batched [`enqueue_cancellable`]: the whole fan-out held back for
/// [`CANCELLATION_GRACE_SECONDS`] so an immediate retraction can
/// [`cancel_many`] it in-queue.
pub async fn enqueue_many_cancellable<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    inbox_urls: &[String],
    activity: &Value,
) -> Result<(), DbError> {
    enqueue_many_inner(
        pool,
        Some(account_id),
        inbox_urls,
        activity,
        false,
        CANCELLATION_GRACE_SECONDS,
    )
    .await
}

/// Batched [`enqueue_many`] within a caller-provided transaction, so the fan-out
/// commits atomically with a related state change. Self-destruct queues an
/// account's whole `Delete(Actor)` fan-out and marks it suspended together, so a
/// crash mid-broadcast rolls back with no partial prefix to re-enqueue and no
/// duplicate deliveries. The batch's `pg_notify` fires on commit.
pub async fn enqueue_many_tx<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    inbox_urls: &[String],
    activity: &Value,
) -> Result<(), DbError> {
    enqueue_many_inner(executor, Some(account_id), inbox_urls, activity, false, 0.0).await
}

/// [`enqueue_many_tx`] with Mastodon's followers-synchronization header, for the
/// followers-only status fan-out inside the status-creation transaction (QC
/// audit #18).
pub async fn enqueue_many_synchronized_tx<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    inbox_urls: &[String],
    activity: &Value,
) -> Result<(), DbError> {
    enqueue_many_inner(executor, Some(account_id), inbox_urls, activity, true, 0.0).await
}

/// Enqueues one activity signed by `account_id` within a caller-provided
/// transaction, so a delivery commits atomically with the durable domain write
/// that produced it — the transactional-outbox guarantee for status creation
/// and relationship mutations. Returns the job id. The
/// `pg_notify` wake-up fires only on commit.
pub async fn enqueue_tx<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(executor, Some(account_id), inbox_url, activity, false, 0.0).await
}

/// [`enqueue_from_instance`] within a caller-provided transaction — the instance
/// actor signs, so the reporting user stays anonymous.
pub async fn enqueue_from_instance_tx<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    inbox_url: &str,
    activity: &Value,
) -> Result<i64, DbError> {
    enqueue_inner(executor, None, inbox_url, activity, false, 0.0).await
}

/// One row of [`enqueue_batch_tx`]: an activity, the inbox it goes to, and the
/// local account that signs it.
#[derive(Debug, Clone)]
pub struct QueuedDelivery {
    pub signer_account_id: i64,
    pub inbox_url: String,
    pub activity: Value,
}

/// Enqueues one *distinct* activity per row in a single statement — the
/// batched form for fan-outs where every payload (and possibly every signer)
/// differs: a severed follow's `Undo`/`Reject` embeds its own edge URI, a
/// `Move` replay's `Follow` is signed by each re-pointed follower (N+1
/// close-out). Job ids are minted in call order, so the deliveries
/// drain in the order the rows were pushed. One NOTIFY wakes the workers for
/// the whole batch.
pub async fn enqueue_batch_tx<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    entries: &[QueuedDelivery],
) -> Result<(), DbError> {
    if entries.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = entries.iter().map(|_| id::next()).collect();
    let signers: Vec<i64> = entries.iter().map(|e| e.signer_account_id).collect();
    let inbox_urls: Vec<String> = entries.iter().map(|e| e.inbox_url.clone()).collect();
    let activities: Vec<Value> = entries.iter().map(|e| e.activity.clone()).collect();
    sqlx::query_scalar!(
        r#"
        WITH inserted AS (
            INSERT INTO delivery_jobs
                (id, account_id, inbox_url, activity, synchronize_followers, run_at)
            SELECT j.id, j.signer, j.inbox_url, j.activity, false, now()
            FROM unnest($1::bigint[], $2::bigint[], $3::text[], $4::jsonb[])
                AS j(id, signer, inbox_url, activity)
            RETURNING id
        ),
        notified AS (
            SELECT pg_notify($5, min(id)::text) FROM inserted
        )
        SELECT count(*) AS "count!" FROM inserted, notified
        "#,
        &ids,
        &signers,
        &inbox_urls,
        &activities,
        CHANNEL,
    )
    .fetch_one(executor)
    .await?;
    Ok(())
}

async fn enqueue_many_inner<'e, E>(
    executor: E,
    account_id: Option<i64>,
    inbox_urls: &[String],
    activity: &Value,
    synchronize_followers: bool,
    delay_seconds: f64,
) -> Result<(), DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    if inbox_urls.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = inbox_urls.iter().map(|_| id::next()).collect();
    // One NOTIFY for the whole batch: workers treat it as a pure wake-up and
    // claim due jobs in batches, so per-row notifications add nothing.
    sqlx::query_scalar!(
        r#"
        WITH inserted AS (
            INSERT INTO delivery_jobs
                (id, account_id, inbox_url, activity, synchronize_followers, run_at)
            SELECT j.id, $3, j.inbox_url, $4, $5, now() + make_interval(secs => $6)
            FROM unnest($1::bigint[], $2::text[]) AS j(id, inbox_url)
            RETURNING id
        ),
        notified AS (
            SELECT pg_notify($7, min(id)::text) FROM inserted
        )
        SELECT count(*) AS "count!" FROM inserted, notified
        "#,
        &ids,
        inbox_urls,
        account_id,
        activity,
        synchronize_followers,
        delay_seconds,
        CHANNEL,
    )
    .fetch_one(executor)
    .await?;
    Ok(())
}

async fn enqueue_inner<'e, E>(
    executor: E,
    account_id: Option<i64>,
    inbox_url: &str,
    activity: &Value,
    synchronize_followers: bool,
    delay_seconds: f64,
) -> Result<i64, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    let job_id = sqlx::query_scalar!(
        r#"
        WITH inserted AS (
            INSERT INTO delivery_jobs
                (id, account_id, inbox_url, activity, synchronize_followers, run_at)
            VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))
            RETURNING id
        ),
        notified AS (
            SELECT pg_notify($7, id::text) FROM inserted
        )
        SELECT inserted.id AS "id!" FROM inserted, notified
        "#,
        id::next(),
        account_id,
        inbox_url,
        activity,
        synchronize_followers,
        delay_seconds,
        CHANNEL,
    )
    .fetch_one(executor)
    .await?;
    Ok(job_id)
}

/// Cancels any queued delivery of the activity with id `activity_uri` to
/// `inbox_url`, returning the cancelled job's attempt count. `Some(0)` means
/// it was never attempted — the remote cannot have seen the activity, so a
/// retraction of it need not be delivered at all. Removing a claimed or
/// retry-pending job is also what keeps a failed `Announce` from being
/// re-delivered *after* its `Undo`.
pub async fn cancel<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    inbox_url: &str,
    activity_uri: &str,
) -> Result<Option<i32>, DbError> {
    let attempts = sqlx::query_scalar!(
        r#"
        DELETE FROM delivery_jobs
        WHERE inbox_url = $1 AND activity->>'id' = $2
        RETURNING attempts
        "#,
        inbox_url,
        activity_uri,
    )
    .fetch_all(pool)
    .await?;
    Ok(attempts.into_iter().max())
}

/// Batched [`cancel`]: removes any queued deliveries of `activity_uri` to the
/// given inboxes, returning each affected inbox with its cancelled job's
/// attempt count (the max, matching [`cancel`]). An inbox mapping to `0`
/// never had the activity attempted, so a retraction toward it can be
/// skipped entirely.
pub async fn cancel_many<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    inbox_urls: &[String],
    activity_uri: &str,
) -> Result<HashMap<String, i32>, DbError> {
    let rows = sqlx::query!(
        r#"
        DELETE FROM delivery_jobs
        WHERE inbox_url = ANY($1) AND activity->>'id' = $2
        RETURNING inbox_url, attempts
        "#,
        inbox_urls,
        activity_uri,
    )
    .fetch_all(pool)
    .await?;
    let mut attempts: HashMap<String, i32> = HashMap::with_capacity(rows.len());
    for row in rows {
        attempts
            .entry(row.inbox_url)
            .and_modify(|max| *max = (*max).max(row.attempts))
            .or_insert(row.attempts);
    }
    Ok(attempts)
}

/// Whether a claimed job still exists — [`cancel`] may have deleted it
/// between the claim and the send, and a cancelled job must not go out.
pub async fn still_queued(pool: &PgPool, job_id: i64) -> Result<bool, DbError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM delivery_jobs WHERE id = $1) AS "exists!""#,
        job_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// A dedicated LISTEN connection for delivery wake-ups. It lives outside the
/// pool because it holds its connection for the process lifetime.
pub async fn listener(pool: &PgPool) -> Result<PgListener, DbError> {
    let options = (*pool.connect_options()).clone();
    let dedicated = sqlx::pool::PoolOptions::new()
        .max_connections(1)
        .connect_lazy_with(options);
    let mut listener = PgListener::connect_with(&dedicated).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

/// Claims up to `limit` due jobs, bumping their attempt counter and leasing
/// them for [`LEASE_SECONDS`].
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<DeliveryJob>, DbError> {
    let jobs = sqlx::query_as!(
        DeliveryJob,
        r#"
        WITH locked AS (
            SELECT id, run_at AS original_run_at
            FROM delivery_jobs
            WHERE run_at <= now()
            ORDER BY run_at, id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        ),
        updated AS (
        UPDATE delivery_jobs SET
            attempts = attempts + 1,
            run_at = now() + make_interval(secs => $2)
        FROM locked
        WHERE delivery_jobs.id = locked.id
        RETURNING delivery_jobs.id, account_id, inbox_url, activity,
                  synchronize_followers, attempts, locked.original_run_at
        )
        SELECT id, account_id, inbox_url, activity, synchronize_followers, attempts
        FROM updated
        ORDER BY original_run_at, id
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// How long until the oldest queued job becomes due. `None` means the queue is
/// empty, so the worker can wait solely on NOTIFY.
pub async fn next_due_delay(pool: &PgPool) -> Result<Option<Duration>, DbError> {
    let seconds = sqlx::query_scalar!(
        r#"SELECT EXTRACT(EPOCH FROM min(run_at) - now())::float8 AS "seconds?" FROM delivery_jobs"#
    )
    .fetch_one(pool)
    .await?;
    Ok(seconds.map(|s| Duration::from_secs_f64(s.max(0.0))))
}

/// Removes a delivered (or permanently failed) job.
pub async fn complete(pool: &PgPool, job_id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM delivery_jobs WHERE id = $1", job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reschedules a failed job with exponential backoff.
pub async fn retry_later(pool: &PgPool, job_id: i64, attempts: i32) -> Result<(), DbError> {
    let delay = backoff_seconds(attempts);
    sqlx::query!(
        "UPDATE delivery_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        job_id,
        delay,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Reschedules a rate-limited job at the later of the remote's `Retry-After`
/// and the normal backoff — a 429 must never be retried sooner than the
/// remote asked. The delay is capped at 24h so a hostile header cannot park
/// a job forever.
pub async fn retry_no_sooner_than(
    pool: &PgPool,
    job_id: i64,
    attempts: i32,
    min_delay_seconds: f64,
) -> Result<(), DbError> {
    let delay = backoff_seconds(attempts)
        .max(min_delay_seconds)
        .min(86_400.0);
    sqlx::query!(
        "UPDATE delivery_jobs SET run_at = now() + make_interval(secs => $2) WHERE id = $1",
        job_id,
        delay,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 60s, 4m, 16m, ~1h, ~4h, capped at 6h between attempts.
fn backoff_seconds(attempts: i32) -> f64 {
    let exp = u32::try_from(attempts.clamp(1, 9)).unwrap_or(1) - 1;
    f64::from((60u32 * 4u32.saturating_pow(exp)).min(21_600))
}

/// Number of jobs due right now — the queue's live backlog, ignoring
/// backed-off retries parked in the future (a dead peer must not read as
/// "under load" forever).
pub async fn due_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM delivery_jobs WHERE run_at <= now()"#
    )
    .fetch_one(pool)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Number of jobs not yet delivered (diagnostics / tests).
pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM delivery_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// One host's slice of the delivery queue, for the `federation queue inspect`
/// CLI: how many jobs are parked for it, the highest retry count among them,
/// and how many seconds until the soonest becomes due (negative once overdue).
pub struct QueueHost {
    pub host: String,
    pub jobs: i64,
    pub max_attempts: i32,
    pub next_due_seconds: Option<f64>,
}

/// Per-host breakdown of the delivery queue, worst backlog first — a read-only
/// diagnostic for the debug CLI. The host is the inbox URL's authority
/// (`https://host:port/path` → `host`); capped at `limit` rows.
pub async fn queue_by_host(pool: &PgPool, limit: i64) -> Result<Vec<QueueHost>, DbError> {
    let rows = sqlx::query_as!(
        QueueHost,
        r#"SELECT
              split_part(split_part(inbox_url, '/', 3), ':', 1) AS "host!",
              count(*) AS "jobs!",
              max(attempts) AS "max_attempts!",
              EXTRACT(EPOCH FROM min(run_at) - now())::float8 AS "next_due_seconds"
           FROM delivery_jobs
           GROUP BY 1
           ORDER BY 2 DESC, 1
           LIMIT $1"#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Test hook: makes every job due now (so tests need not wait out leases).
pub async fn make_all_due(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!("UPDATE delivery_jobs SET run_at = now()")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn local_account(pool: &PgPool) -> i64 {
        account::create_local(
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
        .id
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!((backoff_seconds(1) - 60.0).abs() < f64::EPSILON);
        assert!((backoff_seconds(2) - 240.0).abs() < f64::EPSILON);
        assert!(backoff_seconds(3) > backoff_seconds(2));
        assert!((backoff_seconds(8) - 21_600.0).abs() < f64::EPSILON);
        assert!((backoff_seconds(100) - 21_600.0).abs() < f64::EPSILON);
    }

    #[sqlx::test]
    async fn enqueue_claim_complete_lifecycle(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let activity = json!({"type": "Accept"});
        enqueue(&pool, account_id, "https://remote.example/inbox", &activity)
            .await
            .unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 1);

        let jobs = claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].attempts, 1);
        assert_eq!(jobs[0].inbox_url, "https://remote.example/inbox");
        assert_eq!(jobs[0].activity, activity);

        // The claim leased the job: nothing else is due.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        complete(&pool, jobs[0].id).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn enqueue_notifies_delivery_listener(pool: PgPool) {
        let mut listener = listener(&pool).await.unwrap();
        let account_id = local_account(&pool).await;
        let job_id = enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"type": "Accept"}),
        )
        .await
        .unwrap();

        let message = tokio::time::timeout(Duration::from_secs(1), listener.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.channel(), CHANNEL);
        assert_eq!(message.payload(), job_id.to_string());
    }

    #[sqlx::test]
    async fn next_due_delay_tracks_empty_due_and_leased_jobs(pool: PgPool) {
        assert_eq!(next_due_delay(&pool).await.unwrap(), None);

        let account_id = local_account(&pool).await;
        enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"type": "Accept"}),
        )
        .await
        .unwrap();
        assert!(
            next_due_delay(&pool).await.unwrap().unwrap() <= Duration::from_secs(1),
            "new jobs are due immediately"
        );

        let jobs = claim_due(&pool, 1).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            next_due_delay(&pool).await.unwrap().unwrap() > Duration::from_secs(250),
            "claimed jobs are hidden until the lease expires"
        );
    }

    #[sqlx::test]
    async fn retry_reschedules_and_lease_expiry_resurfaces(pool: PgPool) {
        let account_id = local_account(&pool).await;
        enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({}),
        )
        .await
        .unwrap();

        let jobs = claim_due(&pool, 10).await.unwrap();
        retry_later(&pool, jobs[0].id, jobs[0].attempts)
            .await
            .unwrap();
        // Backed off into the future…
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());
        // …but still queued, and claimable once due again.
        assert_eq!(pending_count(&pool).await.unwrap(), 1);
        make_all_due(&pool).await.unwrap();
        let again = claim_due(&pool, 10).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].attempts, 2);
    }

    #[sqlx::test]
    async fn cancellable_jobs_wait_out_the_grace_and_cancel_in_queue(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let activity = json!({"id": "https://plamenu.test/users/alice#likes/1", "type": "Like"});
        let job_id =
            enqueue_cancellable(&pool, account_id, "https://remote.example/inbox", &activity)
                .await
                .unwrap();

        // Not due during the grace, but visible and queued.
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());
        assert!(still_queued(&pool, job_id).await.unwrap());

        // Cancelling reports it was never attempted and removes it.
        let attempts = cancel(
            &pool,
            "https://remote.example/inbox",
            "https://plamenu.test/users/alice#likes/1",
        )
        .await
        .unwrap();
        assert_eq!(attempts, Some(0));
        assert!(!still_queued(&pool, job_id).await.unwrap());
        assert_eq!(pending_count(&pool).await.unwrap(), 0);

        // Cancelling something no longer queued reports nothing.
        assert_eq!(
            cancel(
                &pool,
                "https://remote.example/inbox",
                "https://plamenu.test/users/alice#likes/1",
            )
            .await
            .unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn relationship_responses_wait_for_the_remote_commit(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let job_id = enqueue_relationship_response(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"type": "Accept", "object": {"type": "Follow"}}),
        )
        .await
        .unwrap();

        assert!(claim_due(&pool, 10).await.unwrap().is_empty());
        assert!(still_queued(&pool, job_id).await.unwrap());
        make_all_due(&pool).await.unwrap();
        assert_eq!(claim_due(&pool, 10).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn cancel_after_claim_reports_the_attempt(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let activity = json!({"id": "https://plamenu.test/users/alice/statuses/9/activity"});
        enqueue(&pool, account_id, "https://remote.example/inbox", &activity)
            .await
            .unwrap();

        let jobs = claim_due(&pool, 10).await.unwrap();
        assert_eq!(jobs.len(), 1);

        // The claim may mean the activity is on the wire: the caller must
        // still deliver the retraction.
        let attempts = cancel(
            &pool,
            "https://remote.example/inbox",
            "https://plamenu.test/users/alice/statuses/9/activity",
        )
        .await
        .unwrap();
        assert_eq!(attempts, Some(1));
        assert!(!still_queued(&pool, jobs[0].id).await.unwrap());
    }

    #[sqlx::test]
    async fn cancel_is_scoped_to_the_inbox(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let activity = json!({"id": "https://plamenu.test/users/alice#likes/2"});
        enqueue_cancellable(&pool, account_id, "https://a.example/inbox", &activity)
            .await
            .unwrap();
        enqueue_cancellable(&pool, account_id, "https://b.example/inbox", &activity)
            .await
            .unwrap();

        assert_eq!(
            cancel(
                &pool,
                "https://a.example/inbox",
                "https://plamenu.test/users/alice#likes/2",
            )
            .await
            .unwrap(),
            Some(0)
        );
        assert_eq!(pending_count(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn claim_respects_limit_and_order(pool: PgPool) {
        let account_id = local_account(&pool).await;
        for i in 0..3 {
            enqueue(
                &pool,
                account_id,
                &format!("https://r{i}.example/inbox"),
                &json!({}),
            )
            .await
            .unwrap();
        }
        let first = claim_due(&pool, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].inbox_url, "https://r0.example/inbox");
        assert_eq!(first[1].inbox_url, "https://r1.example/inbox");

        let second = claim_due(&pool, 2).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].inbox_url, "https://r2.example/inbox");
    }
}
