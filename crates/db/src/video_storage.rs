//! Durable native-video storage admission. A reservation survives deletion of
//! its media/draft and is refunded only after acknowledged file deletion.

use crate::DbError;
use sqlx::{PgConnection, PgPool};

// Shared with source admission; take before any media/account row locks.
const ADMISSION_LOCK: i32 = 0x564f_4401;
pub const COMPAT_POSTER_BYTES: i64 = 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Limits {
    pub per_account: i64,
    pub global: i64,
}

pub async fn lock(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query!("SELECT pg_advisory_xact_lock($1, 0)", ADMISSION_LOCK)
        .execute(conn)
        .await?;
    Ok(())
}

/// Upgrade ready pre-ledger attempts that have no compatibility poster yet.
/// Like the migration's existing-file backfill, this grandfathers existing
/// media, even above a newly configured quota. It grants only one bounded
/// poster allowance per ready attempt, never permission for new uploads.
/// Run recovery before enabling admission. Concurrent recovery is idempotent.
pub async fn recover_missing_posters(pool: &PgPool) -> Result<usize, DbError> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let rows = sqlx::query!(
        "SELECT m.id, d.account_id, d.generation,
            replace(o.file_name, '.draft.mp4', '.draft.poster.jpg') AS \"key!\"
        FROM media_attachments m
        JOIN video_drafts d ON d.media_id = m.id
        JOIN video_draft_outputs o ON o.media_id = m.id AND o.file_name = m.file_name
            AND o.generation = d.generation AND NOT o.retired
        WHERE d.state = 'ready' AND m.processing = 'complete'
            AND o.file_name LIKE '%.draft.mp4'
            AND NOT EXISTS (SELECT 1 FROM video_compatibility_posters p WHERE p.media_id = m.id)
            AND NOT EXISTS (SELECT 1 FROM video_draft_outputs p
                WHERE p.file_name = replace(o.file_name, '.draft.mp4', '.draft.poster.jpg'))
        ORDER BY m.id LIMIT 32 FOR UPDATE OF m"
    )
    .fetch_all(&mut *tx)
    .await?;
    for row in &rows {
        sqlx::query!(
            "INSERT INTO video_storage_reservations
            (file_name, account_id, media_id, reserved_bytes) VALUES ($1, $2, $3, $4)",
            row.key,
            row.account_id,
            row.id,
            COMPAT_POSTER_BYTES
        )
        .execute(&mut *tx)
        .await?;
        let cleanup_id = crate::id::next();
        sqlx::query!(
            "INSERT INTO video_draft_outputs
            (file_name, media_id, generation, cleanup_job_id) VALUES ($1, $2, $3, $4)",
            row.key,
            row.id,
            row.generation,
            cleanup_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(rows.len())
}

/// Caller holds `lock`, acquired BEFORE any row fence. No bytes may be written
/// on a refused or uncertain reservation commit. Keys are never reused.
pub async fn reserve_locked(
    conn: &mut PgConnection,
    owner: i64,
    media_id: i64,
    keys: &[String],
    bytes: &[i64],
    limits: &Limits,
) -> Result<bool, DbError> {
    if keys.is_empty() || keys.len() != bytes.len() || bytes.iter().any(|b| *b <= 0) {
        return Ok(false);
    }
    let Some(additional) = bytes
        .iter()
        .try_fold(0i64, |sum, value| sum.checked_add(*value))
    else {
        return Ok(false);
    };
    let usage = sqlx::query!(
        r#"SELECT
        COALESCE(sum(reserved_bytes), 0)::bigint AS "total!",
        COALESCE(sum(reserved_bytes) FILTER (WHERE account_id = $1), 0)::bigint AS "owned!",
        count(*) FILTER (WHERE reserved_bytes IS NULL) AS "unknown!"
        FROM video_storage_reservations"#,
        owner
    )
    .fetch_one(&mut *conn)
    .await?;
    if usage.unknown > 0
        || additional > limits.per_account.saturating_sub(usage.owned)
        || additional > limits.global.saturating_sub(usage.total)
    {
        return Ok(false);
    }
    sqlx::query!("INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
        SELECT input.file_name, $2, $3, input.bytes FROM unnest($1::text[], $4::bigint[]) AS input(file_name, bytes)",
        keys, owner, media_id, bytes).execute(conn).await?;
    Ok(true)
}

/// Only shrink a known reservation after a fenced, verified write. Admission
/// does not need its global lock for a shrink: concurrent readers overcount.
pub async fn shrink(conn: &mut PgConnection, key: &str, bytes: i64) -> Result<bool, DbError> {
    Ok(sqlx::query!(
        "UPDATE video_storage_reservations SET reserved_bytes = $2::bigint
        WHERE file_name = $1 AND $2::bigint >= 0 AND reserved_bytes >= $2::bigint",
        key,
        bytes
    )
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

/// This key was actually removed (`NotFound` is also success). Exhausted cleanup
/// jobs MUST NOT use this path. Delete the charge and cleanup job atomically.
pub async fn complete_deleted(pool: &PgPool, ids: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "WITH deleted AS (DELETE FROM media_cleanup_jobs WHERE id = ANY($1) RETURNING file_name)
        DELETE FROM video_storage_reservations WHERE file_name IN (SELECT file_name FROM deleted)",
        ids
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Recovery candidates for pre-ledger attempts. The caller must acquire the
/// media row fence before stat/open and recording a size; a live final writer
/// may otherwise race that observation. Missing files measure as zero bytes.
pub async fn unmeasured(pool: &PgPool) -> Result<Vec<(String, i64)>, DbError> {
    let rows = sqlx::query!(
        "SELECT file_name, media_id FROM video_storage_reservations
        WHERE reserved_bytes IS NULL ORDER BY media_id, file_name LIMIT 32"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.file_name, r.media_id))
        .collect())
}

/// Include charged keys in reconciliation even if a file is already absent.
/// Only the cleanup worker's successful deletion/NotFound acknowledgment
/// refunds them; listing a directory is not sufficient evidence.
pub async fn keys(pool: &PgPool) -> Result<Vec<String>, DbError> {
    Ok(
        sqlx::query_scalar!("SELECT file_name FROM video_storage_reservations")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn measure(conn: &mut PgConnection, key: &str, bytes: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE video_storage_reservations SET reserved_bytes = $2::bigint
        WHERE file_name = $1 AND reserved_bytes IS NULL AND $2::bigint >= 0",
        key,
        bytes
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn reserve(pool: &PgPool, owner: i64, key: &str, bytes: i64, limits: Limits) -> bool {
        let mut tx = pool.begin().await.unwrap();
        lock(&mut tx).await.unwrap();
        let accepted = reserve_locked(&mut tx, owner, 42, &[key.to_owned()], &[bytes], &limits)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        accepted
    }

    #[sqlx::test]
    async fn concurrent_accounts_cannot_oversubscribe_the_global_budget(pool: PgPool) {
        let limits = Limits {
            per_account: 100,
            global: 100,
        };
        let (a, b) = tokio::join!(
            reserve(&pool, 1, "a.orig", 70, limits),
            reserve(&pool, 2, "b.orig", 70, limits)
        );
        assert_ne!(a, b);
        let charged: i64 = sqlx::query_scalar(
            "SELECT sum(reserved_bytes)::bigint FROM video_storage_reservations",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(charged, 70);
    }

    #[sqlx::test]
    async fn account_budget_counts_all_keys_and_unknown_sizes_fail_closed(pool: PgPool) {
        let limits = Limits {
            per_account: 100,
            global: 1000,
        };
        assert!(reserve(&pool, 1, "source.orig", 80, limits).await);
        assert!(!reserve(&pool, 1, "output.draft.mp4", 21, limits).await);
        assert!(reserve(&pool, 2, "another.orig", 80, limits).await);
        sqlx::query("UPDATE video_storage_reservations SET reserved_bytes = NULL WHERE file_name = 'another.orig'").execute(&pool).await.unwrap();
        assert!(!reserve(&pool, 3, "new.orig", 1, limits).await);
        let mut tx = pool.begin().await.unwrap();
        measure(&mut tx, "another.orig", 80).await.unwrap();
        assert!(!shrink(&mut tx, "source.orig", 81).await.unwrap());
        assert!(shrink(&mut tx, "source.orig", 60).await.unwrap());
        tx.commit().await.unwrap();
        assert!(reserve(&pool, 1, "output.draft.mp4", 40, limits).await);
    }

    #[sqlx::test]
    async fn only_confirmed_deletion_refunds_storage_not_exhausted_cleanup(pool: PgPool) {
        let limits = Limits {
            per_account: 100,
            global: 100,
        };
        assert!(reserve(&pool, 1, "old.orig", 100, limits).await);
        crate::media_cleanup::enqueue_many(&pool, &["old.orig".to_owned()])
            .await
            .unwrap();
        let jobs = crate::media_cleanup::claim_due(&pool, 10).await.unwrap();
        crate::media_cleanup::complete_many(&pool, &[jobs[0].id])
            .await
            .unwrap();
        assert!(
            !reserve(&pool, 1, "next.orig", 1, limits).await,
            "discarded jobs are not successful file deletion"
        );
        crate::media_cleanup::enqueue_many(&pool, &["old.orig".to_owned()])
            .await
            .unwrap();
        let jobs = crate::media_cleanup::claim_due(&pool, 10).await.unwrap();
        complete_deleted(&pool, &[jobs[0].id]).await.unwrap();
        complete_deleted(&pool, &[jobs[0].id]).await.unwrap();
        assert!(reserve(&pool, 1, "next.orig", 100, limits).await);
    }

    #[sqlx::test]
    async fn invalid_sizes_and_overflow_cannot_create_partial_reservations(pool: PgPool) {
        let limits = Limits {
            per_account: i64::MAX,
            global: i64::MAX,
        };
        assert!(!reserve(&pool, 1, "zero.orig", 0, limits).await);
        assert!(!reserve(&pool, 1, "negative.orig", -1, limits).await);
        let mut tx = pool.begin().await.unwrap();
        lock(&mut tx).await.unwrap();
        assert!(
            !reserve_locked(
                &mut tx,
                1,
                42,
                &["a.orig".into(), "b.orig".into()],
                &[i64::MAX, 1],
                &limits
            )
            .await
            .unwrap()
        );
        tx.commit().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM video_storage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
