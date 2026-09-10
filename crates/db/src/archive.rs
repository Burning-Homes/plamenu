//! Full account archive storage — Mastodon's `Backup`.
//!
//! A request becomes a `scheduled` archive; the archive worker claims it
//! (`in_progress`), builds a zip and stores it in the media store, then marks
//! it `finished` with the stored `file_name`/`file_size`. Only the newest
//! archive per account is kept — finishing one sweeps the account's older
//! archives (and their stored files) — and a retention sweep drops any left
//! past the window. The enum-ish `state` column is plain text, like the
//! `bulk_imports`/`email_jobs` queues.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// One account-archive job. `state` is validated by the server layer; the db
/// keeps it as free text.
#[derive(Debug, Clone)]
pub struct AccountArchive {
    pub id: i64,
    pub account_id: i64,
    pub state: String,
    /// Storage key of the produced zip; `None` until the build finishes.
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

impl AccountArchive {
    /// Whether the zip has been built and is downloadable.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == "finished" && self.file_name.is_some()
    }
}

/// Queues a new archive for `account_id`. The caller has already enforced the
/// per-account rate limit ([`created_within`]).
pub async fn create(pool: &PgPool, account_id: i64) -> Result<AccountArchive, DbError> {
    let archive = sqlx::query_as!(
        AccountArchive,
        r#"
        INSERT INTO account_archives (account_id)
        VALUES ($1)
        RETURNING id, account_id, state, file_name, file_size, created_at, finished_at
        "#,
        account_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(archive)
}

/// Atomically enforces the per-account cooldown and queues a new archive only
/// when no archive was created within the last `days`; returns `None` when the
/// cooldown blocks it. A bare [`created_within`]-then-[`create`] races under
/// READ COMMITTED — concurrent requests each observe no recent row (the
/// not-yet-inserted rows cannot be locked) and each schedule a full archive
/// (finding #44). An account-scoped `pg_advisory_xact_lock` serializes requests
/// for the same account instead: the winner checks, inserts, and commits
/// (releasing the lock); the next acquires it, sees the fresh row, and is
/// refused. The advisory key is the account id, a keyspace shared with
/// [`crate::bulk_import::create_with_rows_capped`]; only same-account create
/// transactions ever contend, and only for their brief duration.
pub async fn create_if_none_within(
    pool: &PgPool,
    account_id: i64,
    days: i32,
) -> Result<Option<AccountArchive>, DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", account_id)
        .execute(&mut *tx)
        .await?;
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM account_archives
            WHERE account_id = $1 AND created_at > now() - make_interval(days => $2)
        ) AS "exists!"
        "#,
        account_id,
        days,
    )
    .fetch_one(&mut *tx)
    .await?;
    if exists {
        tx.commit().await?;
        return Ok(None);
    }
    let archive = sqlx::query_as!(
        AccountArchive,
        r#"
        INSERT INTO account_archives (account_id)
        VALUES ($1)
        RETURNING id, account_id, state, file_name, file_size, created_at, finished_at
        "#,
        account_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(archive))
}

/// Whether `account_id` has requested an archive within the last `days` — the
/// rate gate matching Mastodon's `BackupPolicy::MIN_AGE`. Used for the
/// read-only UI hint; the request path enforces the gate atomically via
/// [`create_if_none_within`].
pub async fn created_within(pool: &PgPool, account_id: i64, days: i32) -> Result<bool, DbError> {
    let exists = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM account_archives
            WHERE account_id = $1 AND created_at > now() - make_interval(days => $2)
        ) AS "exists!"
        "#,
        account_id,
        days,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// An archive owned by `account_id`, in any state.
pub async fn find_for_account(
    pool: &PgPool,
    account_id: i64,
    id: i64,
) -> Result<Option<AccountArchive>, DbError> {
    let archive = sqlx::query_as!(
        AccountArchive,
        r#"
        SELECT id, account_id, state, file_name, file_size, created_at, finished_at
        FROM account_archives WHERE id = $1 AND account_id = $2
        "#,
        id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(archive)
}

/// An account's archives, newest first — the settings-page list.
pub async fn recent_for_account(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> Result<Vec<AccountArchive>, DbError> {
    let archives = sqlx::query_as!(
        AccountArchive,
        r#"
        SELECT id, account_id, state, file_name, file_size, created_at, finished_at
        FROM account_archives WHERE account_id = $1 ORDER BY id DESC LIMIT $2
        "#,
        account_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(archives)
}

/// Claims up to `limit` `scheduled` archives, flipping them to `in_progress`
/// and stamping `claimed_at` as the lease marker.
/// `FOR UPDATE SKIP LOCKED` keeps concurrent workers off the same archive.
pub async fn claim_scheduled(pool: &PgPool, limit: i64) -> Result<Vec<AccountArchive>, DbError> {
    let archives = sqlx::query_as!(
        AccountArchive,
        r#"
        UPDATE account_archives SET state = 'in_progress', claimed_at = now()
        WHERE id IN (
            SELECT id FROM account_archives
            WHERE state = 'scheduled'
            ORDER BY id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, account_id, state, file_name, file_size, created_at, finished_at
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(archives)
}

/// Returns crashed archive builds to the queue: an `in_progress` row whose
/// worker died never reaches `finished` and is never re-claimed (the claim only
/// takes `scheduled` rows), so it would strand forever. This
/// flips any `in_progress` archive whose lease has expired — claimed longer than
/// `lease` ago, or (a pre-lease row) never stamped — back to `scheduled` so a
/// worker rebuilds it. `lease` must exceed the longest expected build so a
/// still-running build is never reclaimed under it. Returns how many were
/// requeued.
pub async fn reclaim_stale(pool: &PgPool, lease: std::time::Duration) -> Result<u64, DbError> {
    let result = sqlx::query!(
        r#"
        UPDATE account_archives SET state = 'scheduled', claimed_at = NULL
        WHERE state = 'in_progress'
          AND (claimed_at IS NULL OR claimed_at < now() - make_interval(secs => $1))
        "#,
        lease.as_secs_f64(),
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Marks an archive `finished`, recording the stored zip and stamping
/// `finished_at`. Returns `false` when no row with this id survives — account
/// deletion removed it while the worker was building. The worker
/// then deletes the object it just wrote, so a build that loses the race with a
/// deletion never leaves an untracked private ZIP in the store.
pub async fn mark_finished(
    pool: &PgPool,
    id: i64,
    file_name: &str,
    file_size: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE account_archives
         SET state = 'finished', file_name = $2, file_size = $3, finished_at = now()
         WHERE id = $1",
        id,
        file_name,
        file_size,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Deletes every archive of `account_id` (in any state), returning the stored
/// file names to remove. Called inside the account deletion/purge transaction
/// so a scheduled or in-progress archive cannot outlive the
/// account: the rows go and the caller schedules durable cleanup of the returned
/// files in the same transaction. A build already in flight when this runs
/// finishes into a now-missing row, loses the conditional [`mark_finished`]
/// race, and deletes its own object.
pub async fn delete_for_account(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        "DELETE FROM account_archives WHERE account_id = $1 RETURNING file_name",
        account_id,
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .flatten()
    .collect();
    Ok(files)
}

/// Deletes one archive by id, returning its stored `file_name` (if any) so the
/// caller can remove the file from the store.
pub async fn delete_by_id(pool: &PgPool, id: i64) -> Result<Option<String>, DbError> {
    let file_name = sqlx::query_scalar!(
        "DELETE FROM account_archives WHERE id = $1 RETURNING file_name",
        id,
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(file_name)
}

/// Deletes every archive of `account_id` except `keep_id`, returning the stored
/// file names to remove — Mastodon keeps only the freshest backup.
pub async fn delete_superseded(
    pool: &PgPool,
    account_id: i64,
    keep_id: i64,
) -> Result<Vec<String>, DbError> {
    let files = sqlx::query_scalar!(
        "DELETE FROM account_archives
         WHERE account_id = $1 AND id <> $2
         RETURNING file_name",
        account_id,
        keep_id,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .flatten()
    .collect();
    Ok(files)
}

/// One stale archive selected for retention removal: its id and stored file
/// (if the build ever finished).
#[derive(Debug, Clone)]
pub struct StaleArchive {
    pub id: i64,
    pub file_name: Option<String>,
}

/// Archives past the retention window — Mastodon's `Vacuum::BackupsVacuum`
/// selection — *without* deleting them yet. The worker removes each stored file
/// first and only then deletes the rows whose file is gone
/// ([`delete_by_ids`]), so a transient media-store failure keeps the row for
/// the next sweep to retry rather than orphaning a private zip with no database
/// reference; durable/retryable file cleanup at large is handled separately.
pub async fn stale(pool: &PgPool, days: i32) -> Result<Vec<StaleArchive>, DbError> {
    let stale = sqlx::query_as!(
        StaleArchive,
        "SELECT id, file_name FROM account_archives
         WHERE created_at < now() - make_interval(days => $1)",
        days,
    )
    .fetch_all(pool)
    .await?;
    Ok(stale)
}

/// Deletes the given archive rows once their stored files have been removed.
/// Returns how many rows were deleted.
pub async fn delete_by_ids(pool: &PgPool, ids: &[i64]) -> Result<u64, DbError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query!("DELETE FROM account_archives WHERE id = ANY($1)", ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// The age in whole seconds of the oldest surviving archive, for the
/// "oldest retained object" retention gauge; `None` when none remain.
pub async fn oldest_age_seconds(pool: &PgPool) -> Result<Option<i64>, DbError> {
    let age = sqlx::query_scalar!(
        r#"SELECT EXTRACT(EPOCH FROM now() - min(created_at))::bigint AS "age" FROM account_archives"#
    )
    .fetch_one(pool)
    .await?;
    Ok(age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn seed_account(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn request_claim_finish_lifecycle(pool: PgPool) {
        let account_id = seed_account(&pool, "alice").await;

        assert!(!created_within(&pool, account_id, 6).await.unwrap());
        let archive = create(&pool, account_id).await.unwrap();
        assert_eq!(archive.state, "scheduled");
        assert!(!archive.is_ready());
        // The freshly-created request trips the rate gate.
        assert!(created_within(&pool, account_id, 6).await.unwrap());

        let claimed = claim_scheduled(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].state, "in_progress");
        // Leased: nothing else claimable.
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());

        mark_finished(&pool, archive.id, "archive-1.zip", 4096)
            .await
            .unwrap();
        let finished = find_for_account(&pool, account_id, archive.id)
            .await
            .unwrap()
            .unwrap();
        assert!(finished.is_ready());
        assert_eq!(finished.file_name.as_deref(), Some("archive-1.zip"));
        assert_eq!(finished.file_size, Some(4096));
        assert!(finished.finished_at.is_some());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn keeps_only_the_newest(pool: PgPool) {
        let account_id = seed_account(&pool, "alice").await;
        let old = create(&pool, account_id).await.unwrap();
        mark_finished(&pool, old.id, "old.zip", 1).await.unwrap();
        let new = create(&pool, account_id).await.unwrap();

        let removed = delete_superseded(&pool, account_id, new.id).await.unwrap();
        assert_eq!(removed, vec!["old.zip".to_owned()]);
        assert!(
            find_for_account(&pool, account_id, old.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_for_account(&pool, account_id, new.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn create_if_none_within_enforces_the_cooldown(pool: PgPool) {
        let account_id = seed_account(&pool, "alice").await;

        // The first request through the atomic gate schedules an archive.
        let first = create_if_none_within(&pool, account_id, 6).await.unwrap();
        assert!(matches!(first, Some(a) if a.state == "scheduled"));
        // A second within the window is refused, and nothing else is scheduled.
        assert!(
            create_if_none_within(&pool, account_id, 6)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            recent_for_account(&pool, account_id, 100)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn create_if_none_within_serializes_concurrent_requests(pool: PgPool) {
        let account_id = seed_account(&pool, "alice").await;

        // Fire several requests at once. Without the account-scoped advisory
        // lock a bare check-then-insert lets them all observe no recent row and
        // each schedule an archive (finding #44); with it, exactly one wins and
        // the rest are refused, so a single archive row exists afterward.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                create_if_none_within(&pool, account_id, 6).await.unwrap()
            }));
        }
        let mut scheduled = 0;
        for handle in handles {
            if handle.await.unwrap().is_some() {
                scheduled += 1;
            }
        }
        assert_eq!(scheduled, 1, "exactly one concurrent request is admitted");
        assert_eq!(
            recent_for_account(&pool, account_id, 100)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn reclaim_stale_requeues_only_crashed_builds(pool: PgPool) {
        use std::time::Duration;
        let account_id = seed_account(&pool, "alice").await;
        let archive = create(&pool, account_id).await.unwrap();

        // Claim it: state flips to in_progress and the lease is stamped.
        let claimed = claim_scheduled(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // A fresh claim is inside its lease, so a reclaim leaves it alone — a
        // still-running build must never be requeued out from under its worker.
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            0
        );
        assert!(claim_scheduled(&pool, 10).await.unwrap().is_empty());

        // Age the lease past the window (simulate a crashed worker). Now the
        // reclaim returns it to scheduled and a worker can claim it again.
        sqlx::query!(
            "UPDATE account_archives SET claimed_at = now() - interval '2 hours' WHERE id = $1",
            archive.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            1
        );
        let requeued = find_for_account(&pool, account_id, archive.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requeued.state, "scheduled");
        assert_eq!(claim_scheduled(&pool, 10).await.unwrap().len(), 1);

        // A finished archive is never reclaimed.
        mark_finished(&pool, archive.id, "a.zip", 1).await.unwrap();
        sqlx::query!(
            "UPDATE account_archives SET claimed_at = now() - interval '2 hours' WHERE id = $1",
            archive.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            reclaim_stale(&pool, Duration::from_hours(1)).await.unwrap(),
            0
        );
    }

    /// Account deletion cancels every archive of the account and
    /// hands back the stored files to clean up, and a finish that races a
    /// deletion (its row now gone) reports the loss so the worker can delete the
    /// object it just wrote.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn delete_for_account_cancels_rows_and_conditional_finish(pool: PgPool) {
        let alice = seed_account(&pool, "alice").await;
        let bob = seed_account(&pool, "bob").await;

        // Alice has a finished archive (with a stored file) and a scheduled one.
        let finished = create(&pool, alice).await.unwrap();
        assert!(
            mark_finished(&pool, finished.id, "alice.zip", 10)
                .await
                .unwrap()
        );
        let scheduled = create(&pool, alice).await.unwrap();
        // Bob has one, to prove the cancellation is account-scoped.
        let bobs = create(&pool, bob).await.unwrap();

        // Cancelling Alice removes both her rows and returns the finished file.
        let mut conn = pool.acquire().await.unwrap();
        let files = delete_for_account(&mut conn, alice).await.unwrap();
        assert_eq!(files, vec!["alice.zip".to_owned()]);
        assert!(
            find_for_account(&pool, alice, finished.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_for_account(&pool, alice, scheduled.id)
                .await
                .unwrap()
                .is_none()
        );
        // Bob's archive is untouched.
        assert!(
            find_for_account(&pool, bob, bobs.id)
                .await
                .unwrap()
                .is_some()
        );

        // Finishing an archive whose row was deleted mid-build reports the loss.
        assert!(
            !mark_finished(&pool, finished.id, "late.zip", 10)
                .await
                .unwrap(),
            "a finish into a missing row loses the deletion race"
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn scoping_and_stale_sweep(pool: PgPool) {
        let alice = seed_account(&pool, "alice").await;
        let bob = seed_account(&pool, "bob").await;
        let archive = create(&pool, alice).await.unwrap();
        mark_finished(&pool, archive.id, "a.zip", 1).await.unwrap();

        // Bob can neither see nor delete Alice's archive.
        assert!(
            find_for_account(&pool, bob, archive.id)
                .await
                .unwrap()
                .is_none()
        );

        // Fresh, so the oldest-retained gauge is ~0 while it stands.
        assert!(oldest_age_seconds(&pool).await.unwrap().unwrap() < 60);

        // Ageing it past the retention window selects it, and the two-step
        // sweep (select stale, then delete rows once their files are gone —
        // finding #50) removes the row and reports its file.
        sqlx::query!(
            "UPDATE account_archives SET created_at = now() - interval '8 days' WHERE id = $1",
            archive.id,
        )
        .execute(&pool)
        .await
        .unwrap();
        // The aged archive now dominates the oldest-retained gauge.
        assert!(oldest_age_seconds(&pool).await.unwrap().unwrap() > 7 * 86_400);

        let stale = stale(&pool, 7).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, archive.id);
        assert_eq!(stale[0].file_name.as_deref(), Some("a.zip"));

        assert_eq!(delete_by_ids(&pool, &[archive.id]).await.unwrap(), 1);
        assert!(
            find_for_account(&pool, alice, archive.id)
                .await
                .unwrap()
                .is_none()
        );
        // Nothing remains, so there is no oldest object, and an empty delete is
        // a no-op.
        assert_eq!(oldest_age_seconds(&pool).await.unwrap(), None);
        assert_eq!(delete_by_ids(&pool, &[]).await.unwrap(), 0);
    }
}
