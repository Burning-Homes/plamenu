//! Durable media-file cleanup worker.
//!
//! Drains the `media_cleanup_jobs` queue: each job names a stored file that a
//! status/account deletion orphaned (its database row is already gone), and the
//! worker removes the file from the [`MediaStore`](crate::storage::MediaStore).
//! The queue is leased (see `docs/QUEUES.md`), so a crash — or a transient store
//! error — simply lets the job become due again and the removal is retried; a job
//! that keeps failing is dropped once it exhausts its attempts, rather than
//! looping forever.
//!
//! It also hosts the reconciliation sweep ([`reconcile`]) that catches files
//! orphaned by deletions that ran *before* this queue existed: it lists the store
//! and enqueues any key no database row references.

use std::collections::HashSet;

use plamenu_db::media_cleanup;
use tokio::task::JoinHandle;

use crate::AppState;

/// How many cleanup jobs to claim per wake-up.
const BATCH_SIZE: i64 = 100;
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// Claims and processes one batch of due cleanup jobs; returns how many were
/// claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match media_cleanup::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim media cleanup jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    // Successes complete in one statement after the batch; only failures stay
    // leased (and cost no statement at all).
    let mut done: Vec<i64> = Vec::with_capacity(jobs.len());
    for job in jobs {
        // A job reclaimed past the cap can never be removed (a permanently
        // unreadable path); drop it after logging rather than loop forever.
        if job.exhausted() {
            tracing::warn!(
                file = %job.file_name,
                attempts = job.attempts,
                "dropping media cleanup after too many failed attempts"
            );
            done.push(job.id);
            continue;
        }
        // The claim leased the job; delete the file, then complete it. A missing
        // file is not an error, so an already-gone key completes cleanly. A real
        // store error leaves the job leased so it retries after the lease expires.
        match state.media.delete(&job.file_name).await {
            Ok(()) => done.push(job.id),
            Err(error) => {
                tracing::warn!(
                    %error,
                    file = %job.file_name,
                    "media cleanup delete failed; will retry after the lease expires"
                );
            }
        }
    }
    if let Err(error) = media_cleanup::complete_many(&state.pool, &done).await {
        tracing::error!(%error, "failed to complete media cleanup jobs");
    }
    claimed
}

/// What a reconciliation sweep found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Files present in the store.
    pub scanned: usize,
    /// Distinct keys still referenced by some database row.
    pub referenced: usize,
    /// Stored files no database row references — orphans.
    pub orphans: usize,
    /// Orphans enqueued for deletion (0 in dry-run mode).
    pub enqueued: usize,
}

/// Reconciliation sweep: finds files in the store that no database row
/// references — the orphans left behind by deletions that ran before the cleanup
/// queue existed — and, when `apply` is set, enqueues them for durable deletion
/// through the same worker. Dry-run (`apply == false`) only counts them.
///
/// Safety: the referenced-key set is the exhaustive union of every file-name
/// column (see [`media_cleanup::referenced_keys`]), so a live file is never
/// mistaken for an orphan. The one residual race is a brand-new upload whose file
/// is written just before its database row is inserted; run this during low
/// upload activity, and note that `apply` only *enqueues* — the leased worker
/// still deletes asynchronously.
pub async fn reconcile(
    state: &AppState,
    apply: bool,
) -> Result<ReconcileReport, crate::error::ApiError> {
    let stored = state
        .media
        .list()
        .await
        .map_err(|error| crate::error::ApiError::Internal(Box::new(error)))?;
    let referenced: HashSet<String> = media_cleanup::referenced_keys(&state.pool)
        .await?
        .into_iter()
        .collect();
    let orphans: Vec<String> = stored
        .iter()
        .filter(|key| !referenced.contains(*key))
        .cloned()
        .collect();
    let enqueued = if apply && !orphans.is_empty() {
        media_cleanup::enqueue_many(&state.pool, &orphans).await?;
        orphans.len()
    } else {
        0
    };
    Ok(ReconcileReport {
        scanned: stored.len(),
        referenced: referenced.len(),
        orphans: orphans.len(),
        enqueued,
    })
}

/// Runs the cleanup loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("media cleanup worker started");
        loop {
            if Box::pin(run_due(&state)).await == 0
                && !crate::workers::pause(&state, IDLE_POLL).await
            {
                return;
            }
        }
    })
}
