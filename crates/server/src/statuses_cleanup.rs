//! The auto-deletion sweep: Mastodon's
//! `Scheduler::AccountsStatusesCleanupScheduler` plus its per-account
//! `AccountStatusesCleanupService`, folded into one budgeted pass. Deletions
//! go through [`actions::delete_status`], so each one federates a `Delete`
//! (or `Undo(Announce)` for a boost), publishes the streaming event and
//! cascades exactly like a manual delete.
//!
//! The run budget is deliberately small — the point of the scheduler-side cap
//! is to be nice to the fediverse at large (every deletion fans out to
//! followers' servers). Mastodon budgets 5 per push thread up to 300;
//! Plamenu's single sequential delivery worker gets a flat cap instead.

use std::time::Duration;

use plamenu_db::statuses_cleanup::{self, CleanupPolicy};
use plamenu_db::{account, job};
use tokio::task::JoinHandle;

use crate::{AppState, actions};

/// Mastodon schedules the cleanup scheduler every minute.
const SWEEP_INTERVAL: Duration = Duration::from_mins(1);
/// Deletions per sweep across all accounts.
const MAX_BUDGET: usize = 50;
/// Deletions per account per sweep — spreads the fan-out load across diverse
/// follower sets and lets every enabled user see progress (Mastodon's
/// `PER_ACCOUNT_BUDGET`).
const PER_ACCOUNT_BUDGET: usize = 5;
/// Policies examined per sweep.
const POLICY_PAGE: i64 = 100;
/// Skip the sweep while this many deliveries are already due — the analogue
/// of Mastodon's queue-latency `under_load?` guard.
const BACKLOG_THRESHOLD: u64 = 500;
/// How long a deleted status' tombstone keeps answering `410 Gone`
/// before it is pruned — generous headroom over federated `Delete` propagation.
const TOMBSTONE_RETENTION_DAYS: i32 = 7;

/// One budgeted sweep. `cursor` is the account id the previous sweep stopped
/// at (0 to start); returns `(deleted, next_cursor)`.
pub async fn run_once(state: &AppState, cursor: i64) -> (u64, i64) {
    match job::due_count(&state.pool).await {
        Ok(backlog) if backlog > BACKLOG_THRESHOLD => {
            tracing::debug!(backlog, "delivery queue busy; skipping cleanup sweep");
            return (0, cursor);
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, "cleanup sweep backlog check failed");
            return (0, cursor);
        }
    }

    let mut cursor = cursor;
    let mut budget = MAX_BUDGET;
    let mut total: u64 = 0;
    // Keep re-fetching the (wrap-around) policy page until a whole pass
    // deletes nothing — Mastodon's "loop through all policies at least once
    // until the budget is exhausted".
    loop {
        let policies = match statuses_cleanup::enabled_page(&state.pool, cursor, POLICY_PAGE).await
        {
            Ok(policies) => policies,
            Err(error) => {
                tracing::error!(%error, "cleanup policy page fetch failed");
                break;
            }
        };
        if policies.is_empty() {
            break;
        }
        let mut pass_deleted: u64 = 0;
        for policy in policies {
            let deleted = cleanup_account(state, &policy, budget.min(PER_ACCOUNT_BUDGET)).await;
            budget -= deleted;
            pass_deleted += deleted as u64;
            total += deleted as u64;
            cursor = policy.account_id;
            if budget == 0 {
                return (total, cursor);
            }
        }
        if pass_deleted == 0 {
            break;
        }
    }
    (total, cursor)
}

/// Deletes up to `budget` of one account's eligible statuses and advances the
/// policy's scan cursor (Mastodon's `AccountStatusesCleanupService#call`).
async fn cleanup_account(state: &AppState, policy: &CleanupPolicy, budget: usize) -> usize {
    if budget == 0 {
        return 0;
    }
    let cutoff = match statuses_cleanup::compute_cutoff_id(&state.pool, policy).await {
        Ok(Some(cutoff)) => cutoff,
        Ok(None) => return 0, // nothing old enough yet
        Err(error) => {
            tracing::error!(%error, account = policy.account_id, "cleanup cutoff failed");
            return 0;
        }
    };
    let limit = i64::try_from(budget).unwrap_or(i64::MAX);
    let ids = match statuses_cleanup::statuses_to_delete(&state.pool, policy, cutoff, limit).await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error, account = policy.account_id, "cleanup eligibility failed");
            return 0;
        }
    };
    let author = match account::find_by_id(&state.pool, policy.account_id).await {
        Ok(Some(author)) if author.is_local() => author,
        Ok(_) => return 0,
        Err(error) => {
            tracing::error!(%error, account = policy.account_id, "cleanup author fetch failed");
            return 0;
        }
    };
    let mut deleted = 0;
    let mut last_deleted = None;
    for id in ids {
        // A failed delete is dropped, like Mastodon's RemovalWorker rescuing
        // RecordInvalid; the cursor moves past it either way.
        match actions::delete_status(state, &author, id, actions::DeleteMode::Wipe).await {
            Ok(_) => {
                deleted += 1;
                last_deleted = Some(id);
            }
            Err(error) => {
                tracing::warn!(
                    error = %error.chain(),
                    account = policy.account_id,
                    status = id,
                    "cleanup delete failed"
                );
            }
        }
    }
    let advance_to = last_deleted.unwrap_or(cutoff);
    if let Err(error) =
        statuses_cleanup::record_last_inspected(&state.pool, policy.account_id, advance_to).await
    {
        tracing::error!(%error, account = policy.account_id, "cleanup cursor update failed");
    }
    deleted
}

/// Runs the cleanup sweep every minute until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("statuses cleanup sweeper started");
        let mut cursor = 0_i64;
        loop {
            let (deleted, next_cursor) = run_once(&state, cursor).await;
            cursor = next_cursor;
            if deleted > 0 {
                tracing::info!(deleted, "statuses cleanup sweep");
            }
            // Prune expired delete tombstones on the same maintenance tick.
            match plamenu_db::status_tombstone::prune_expired(&state.pool, TOMBSTONE_RETENTION_DAYS)
                .await
            {
                Ok(pruned) if pruned > 0 => tracing::debug!(pruned, "delete tombstones pruned"),
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "tombstone prune failed"),
            }
            if !crate::workers::pause(&state, SWEEP_INTERVAL).await {
                return;
            }
        }
    })
}
