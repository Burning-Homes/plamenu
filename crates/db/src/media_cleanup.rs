//! Durable media-file cleanup queue.
//!
//! Deleting a status or account removes the `media_attachments` /
//! `media_hls_segments` rows that named its stored files — usually by
//! `ON DELETE CASCADE`, so the file names are gone before any cleanup code could
//! read them. `GET /media/{file}` serves any stored key without a database
//! lookup, so those orphaned bytes stayed publicly retrievable from the origin
//! forever.
//!
//! The fix captures every storage key *before* its row vanishes and enqueues it
//! here in the same transaction as the deletion, then a worker removes the file
//! and deletes the job. This is the lease pattern (see `docs/QUEUES.md`): a claim
//! pushes `run_at` into the future, the worker deletes the job only once the file
//! is gone, and a crash simply lets the lease expire so the deletion is retried.
//! `attempts` bounds a job whose file can never be removed (a permanently
//! unreadable path) so it is eventually dropped instead of looping forever.

use sqlx::{PgConnection, PgExecutor, PgPool};

use crate::{DbError, id};

/// How long a claimed cleanup stays invisible before a crashed (or failed)
/// deletion becomes due again. Short: deleting a file is quick, and a still-live
/// file is a privacy leak we want gone promptly.
const LEASE_SECONDS: f64 = 120.0;

/// Drop a cleanup job once it has been reclaimed this many times — a file that
/// cannot be deleted (permissions, a vanished mount) must not loop forever.
const MAX_ATTEMPTS: i32 = 10;

/// A leased cleanup job: its id (to [`complete`] it) and the store key to delete.
#[derive(Debug, Clone)]
pub struct CleanupJob {
    pub id: i64,
    pub file_name: String,
    /// Times claimed, including this one; `> MAX_ATTEMPTS` means give up.
    pub attempts: i32,
}

impl CleanupJob {
    /// Whether this job has failed too many times to keep retrying.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.attempts > MAX_ATTEMPTS
    }
}

/// Enqueues store keys for durable deletion. Executor-generic so the enqueue
/// commits in the same transaction as the row deletion that orphaned the files —
/// the whole point of the queue is that "row gone" and "file scheduled for
/// removal" are atomic. Blank names are skipped; a key already queued is left in
/// place (the `UNIQUE(file_name)` constraint dedups).
pub async fn enqueue_many<'e, E: PgExecutor<'e>>(
    executor: E,
    file_names: &[String],
) -> Result<(), DbError> {
    let keys: Vec<String> = file_names
        .iter()
        .filter(|name| !name.trim().is_empty())
        .cloned()
        .collect();
    if keys.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = keys.iter().map(|_| id::next()).collect();
    sqlx::query!(
        "INSERT INTO media_cleanup_jobs (id, file_name)
         SELECT input.id, input.file_name
         FROM unnest($1::bigint[], $2::text[]) AS input(id, file_name)
         ON CONFLICT (file_name) DO NOTHING",
        &ids,
        &keys,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Collects every stored file a status's media references — original, preview,
/// and any cached HLS segments. Read *before* the cascade removes the media rows;
/// the caller enqueues the result with [`enqueue_many`] only once it has
/// confirmed the status was actually deleted (so a no-op delete of a status the
/// caller doesn't own never schedules its files for removal).
pub async fn collect_status_keys(
    conn: &mut PgConnection,
    status_id: i64,
) -> Result<Vec<String>, DbError> {
    let attachments = sqlx::query!(
        "SELECT file_name, small_file_name FROM media_attachments WHERE status_id = $1",
        status_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut keys: Vec<String> = Vec::new();
    for row in attachments {
        keys.extend(row.file_name);
        keys.extend(row.small_file_name);
    }
    let hls = sqlx::query_scalar!(
        "SELECT s.cache_file
         FROM media_hls_segments s
         JOIN media_attachments m ON m.id = s.media_id
         WHERE m.status_id = $1",
        status_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    keys.extend(hls);
    Ok(keys)
}

/// [`collect_status_keys`] over a whole status set in two statements — for the
/// moderation sweeps that purge many statuses inside one transaction.
pub async fn collect_status_keys_many(
    conn: &mut PgConnection,
    status_ids: &[i64],
) -> Result<Vec<String>, DbError> {
    if status_ids.is_empty() {
        return Ok(Vec::new());
    }
    let attachments = sqlx::query!(
        "SELECT file_name, small_file_name FROM media_attachments WHERE status_id = ANY($1)",
        status_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut keys: Vec<String> = Vec::new();
    for row in attachments {
        keys.extend(row.file_name);
        keys.extend(row.small_file_name);
    }
    let hls = sqlx::query_scalar!(
        "SELECT s.cache_file
         FROM media_hls_segments s
         JOIN media_attachments m ON m.id = s.media_id
         WHERE m.status_id = ANY($1)",
        status_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    keys.extend(hls);
    Ok(keys)
}

/// Collects every stored file an account owns — all of its attachments'
/// originals/previews, their cached HLS segments, and its own avatar and header.
/// Read inside the account deletion/purge transaction, *before* the media rows
/// are deleted and the avatar/header columns are blanked; the caller enqueues the
/// result with [`enqueue_many`] in the same transaction.
pub async fn collect_account_keys(
    conn: &mut PgConnection,
    account_id: i64,
) -> Result<Vec<String>, DbError> {
    let attachments = sqlx::query!(
        "SELECT file_name, small_file_name FROM media_attachments WHERE account_id = $1",
        account_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut keys: Vec<String> = Vec::new();
    for row in attachments {
        keys.extend(row.file_name);
        keys.extend(row.small_file_name);
    }
    let hls = sqlx::query_scalar!(
        "SELECT s.cache_file
         FROM media_hls_segments s
         JOIN media_attachments m ON m.id = s.media_id
         WHERE m.account_id = $1",
        account_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    keys.extend(hls);
    let profile = sqlx::query!(
        "SELECT avatar_file_name, header_file_name FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(profile) = profile {
        keys.extend(profile.avatar_file_name);
        keys.extend(profile.header_file_name);
    }
    let emoji = sqlx::query_scalar!(
        "SELECT image_file_name AS \"image_file_name!\" FROM custom_emojis
         WHERE owner_account_id = $1 AND image_file_name IS NOT NULL",
        account_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    keys.extend(emoji);
    Ok(keys)
}

/// Claims up to `limit` due cleanup jobs, leasing each for [`LEASE_SECONDS`] and
/// bumping its attempt counter. The row survives until the worker [`complete`]s
/// it, so a crash (or a failed deletion) lets the lease expire and the removal is
/// retried rather than lost.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<CleanupJob>, DbError> {
    let jobs = sqlx::query_as!(
        CleanupJob,
        r#"
        UPDATE media_cleanup_jobs SET
            run_at = now() + make_interval(secs => $2),
            attempts = attempts + 1
        WHERE id IN (
            SELECT id FROM media_cleanup_jobs
            WHERE run_at <= now()
            ORDER BY run_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, file_name, attempts
        "#,
        limit,
        LEASE_SECONDS,
    )
    .fetch_all(pool)
    .await?;
    Ok(jobs)
}

/// Removes a finished (file deleted) or exhausted cleanup job.
pub async fn complete(pool: &PgPool, id: i64) -> Result<(), DbError> {
    sqlx::query!("DELETE FROM media_cleanup_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// [`complete`] for a whole processed batch in one statement — the worker
/// completes its successes together; only failures stay leased.
pub async fn complete_many(pool: &PgPool, ids: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!("DELETE FROM media_cleanup_jobs WHERE id = ANY($1)", ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Number of queued cleanup jobs (diagnostics / tests).
pub async fn pending_count(pool: &PgPool) -> Result<u64, DbError> {
    let count = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM media_cleanup_jobs"#)
        .fetch_one(pool)
        .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Every store key still referenced by *any* database row — the union of every
/// file-name column across the schema (attachments, HLS segments, avatars and
/// headers, custom emoji, preview cards, archives, and site uploads). The
/// reconciliation sweep (finding #39) subtracts this set from the files actually
/// on disk to find orphans left behind by pre-fix deletions; anything absent here
/// is safe to remove. Kept exhaustive on purpose: a missing column would make
/// reconciliation delete a live file, so this is the single source of truth for
/// "a key the product still needs".
pub async fn referenced_keys(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let keys = sqlx::query_scalar!(
        r#"
        SELECT key AS "key!" FROM (
            SELECT file_name AS key FROM media_attachments WHERE file_name IS NOT NULL
            UNION SELECT small_file_name FROM media_attachments WHERE small_file_name IS NOT NULL
            UNION SELECT cache_file FROM media_hls_segments
            UNION SELECT avatar_file_name FROM accounts WHERE avatar_file_name IS NOT NULL
            UNION SELECT header_file_name FROM accounts WHERE header_file_name IS NOT NULL
            UNION SELECT image_file_name FROM custom_emojis WHERE image_file_name IS NOT NULL
            UNION SELECT image_file_name FROM preview_cards WHERE image_file_name IS NOT NULL
            UNION SELECT file_name FROM account_archives WHERE file_name IS NOT NULL
            UNION SELECT file_name FROM site_uploads WHERE file_name IS NOT NULL
            UNION SELECT file_name FROM site_upload_variants WHERE file_name IS NOT NULL
        ) refs
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

    async fn seed_account(pool: &PgPool) -> i64 {
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

    #[sqlx::test]
    async fn enqueue_dedups_claim_leases_and_complete_removes(pool: PgPool) {
        enqueue_many(&pool, &["a.jpg".into(), "b.jpg".into(), "  ".into()])
            .await
            .unwrap();
        // The blank key is skipped; a re-enqueue of an existing key is a no-op.
        enqueue_many(&pool, &["a.jpg".into()]).await.unwrap();
        assert_eq!(pending_count(&pool).await.unwrap(), 2);

        let claimed = claim_due(&pool, 10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(claimed.iter().all(|job| job.attempts == 1));
        // Leased, not deleted: a crash keeps them, and nothing else is claimable.
        assert_eq!(pending_count(&pool).await.unwrap(), 2);
        assert!(claim_due(&pool, 10).await.unwrap().is_empty());

        for job in &claimed {
            complete(&pool, job.id).await.unwrap();
        }
        assert_eq!(pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn expired_lease_resurfaces_and_eventually_exhausts(pool: PgPool) {
        enqueue_many(&pool, &["stuck.jpg".into()]).await.unwrap();
        let first = claim_due(&pool, 10).await.unwrap();
        assert_eq!(first[0].attempts, 1);
        assert!(!first[0].exhausted());

        // Simulate repeated failure: never complete, force the lease to expire.
        for _ in 0..MAX_ATTEMPTS {
            sqlx::query!("UPDATE media_cleanup_jobs SET run_at = now()")
                .execute(&pool)
                .await
                .unwrap();
            let _ = claim_due(&pool, 10).await.unwrap();
        }
        sqlx::query!("UPDATE media_cleanup_jobs SET run_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let done = claim_due(&pool, 10).await.unwrap();
        assert!(done[0].exhausted(), "reclaimed past the cap");
    }

    #[sqlx::test]
    async fn collect_account_keys_captures_attachments_and_profile(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        // An avatar and header on the account.
        sqlx::query!(
            "UPDATE accounts SET avatar_file_name = 'ava.png', header_file_name = 'hdr.png' WHERE id = $1",
            account_id,
        )
        .execute(&pool)
        .await
        .unwrap();
        // An attachment with an original + preview.
        sqlx::query!(
            "INSERT INTO media_attachments (id, account_id, file_name, small_file_name, content_type)
             VALUES ($1, $2, 'orig.jpg', 'small.jpg', 'image/jpeg')",
            id::next(),
            account_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let mut keys = collect_account_keys(&mut conn, account_id).await.unwrap();
        enqueue_many(&mut *conn, &keys).await.unwrap();
        drop(conn);
        keys.sort();
        assert_eq!(
            keys,
            vec!["ava.png", "hdr.png", "orig.jpg", "small.jpg"],
            "every original, preview, avatar, and header is captured",
        );

        let mut queued: Vec<String> = claim_due(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|job| job.file_name)
            .collect();
        queued.sort();
        assert_eq!(queued.len(), 4, "all four keys were enqueued");
    }

    #[sqlx::test]
    async fn purge_local_data_schedules_media_cleanup(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        sqlx::query!(
            "UPDATE accounts SET avatar_file_name = 'ava.png' WHERE id = $1",
            account_id,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO media_attachments (id, account_id, file_name, content_type)
             VALUES ($1, $2, 'post.jpg', 'image/jpeg')",
            id::next(),
            account_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // A self-deletion purge captures the account's files for cleanup in the
        // same transaction that removes their rows.
        account::purge_local_data(&pool, account_id).await.unwrap();

        let mut queued: Vec<String> = claim_due(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|job| job.file_name)
            .collect();
        queued.sort();
        assert_eq!(queued, vec!["ava.png", "post.jpg"]);
        // The media row is gone but its file is scheduled for removal — the
        // orphaned-bytes leak is closed.
        let remaining = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM media_attachments WHERE account_id = $1"#,
            account_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 0);
    }
}
