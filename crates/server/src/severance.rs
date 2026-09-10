//! Domain-block severance — removing a user's follow edges with a blocked
//! domain, off the request path.
//!
//! [`crate::actions::block_domain`] records the block row, wipes the domain's
//! notifications and queues a severance here; the worker severs the follow
//! edges in both directions with set-based deletes and one batched insert of
//! the per-edge retractions (`Undo(Follow)` toward accounts the user followed,
//! `Reject(Follow)` toward their followers — each embeds its own edge URI, so
//! the payloads are distinct). The relationships stay visible for the seconds
//! until the job runs — Mastodon's `AfterAccountDomainBlockWorker` semantics,
//! which every client already tolerates.
//!
//! The deliveries ride the ordinary delivery queue, which owns retries and
//! gives up on its own schedule, so a peer that has vanished forever can never
//! wedge the severance (the chaotic-federation rule).

use std::time::Duration;

use plamenu_ap::activity;
use plamenu_db::domain_severance_job::{self, ClaimedSeverance};
use plamenu_db::{account, follow, id, job};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;

/// Severances claimed per tick. Each is a handful of set-based statements, so
/// the batch can be larger than the move replay's.
const SEVERANCE_BATCH_SIZE: i64 = 5;
/// How long the worker sleeps when the queue is empty.
const SEVERANCE_IDLE_POLL: Duration = Duration::from_secs(10);
/// Attempts before a severance is given up on. The block row stands either
/// way; what would be lost is the edge cleanup, which re-blocking retries.
const SEVERANCE_MAX_ATTEMPTS: i32 = 5;
/// Backoff base; attempt `n` waits `SEVERANCE_RETRY_BASE * 2^n` (1 min → ~32 min).
const SEVERANCE_RETRY_BASE: Duration = Duration::from_mins(1);

/// Claims and runs one batch of due severances; returns how many were claimed
/// (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match domain_severance_job::claim_due(&state.pool, SEVERANCE_BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim domain severance jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        match sever(state, &job).await {
            Ok(()) => complete_severance(state, job.id).await,
            Err(error) => {
                tracing::warn!(
                    error = %error.chain(),
                    account = job.account_id,
                    domain = %job.domain,
                    attempts = job.attempts,
                    "domain severance failed"
                );
                if job.attempts >= SEVERANCE_MAX_ATTEMPTS {
                    complete_severance(state, job.id).await;
                } else {
                    let shift = u32::try_from(job.attempts.clamp(0, 5)).unwrap_or(0);
                    let delay = SEVERANCE_RETRY_BASE.saturating_mul(1 << shift);
                    if let Err(error) =
                        domain_severance_job::reschedule(&state.pool, job.id, delay).await
                    {
                        tracing::error!(%error, "failed to reschedule a domain severance");
                    }
                }
            }
        }
    }
    claimed
}

/// Severs every follow edge between the blocking account and `job.domain`:
/// two edge reads, then one transaction holding the batched retraction
/// enqueue and the two set-based deletes — the edges and the activities that
/// retract them commit together. Statement count is flat in
/// the relationship count. Idempotent: a reclaim finds only edges the
/// previous attempt did not delete.
async fn sever(state: &AppState, job: &ClaimedSeverance) -> Result<(), ApiError> {
    let Some(actor) = account::find_by_id(&state.pool, job.account_id).await? else {
        return Ok(()); // the blocker was deleted; nothing to sever
    };
    let following = follow::edges_to_domain(&state.pool, actor.id, &job.domain).await?;
    let followers = follow::edges_from_domain(&state.pool, actor.id, &job.domain).await?;
    if following.is_empty() && followers.is_empty() {
        return Ok(());
    }

    // Per-edge retractions, exactly what the per-edge pipeline sent: an
    // `Undo(Follow)` embedding our follow's URI toward each account the user
    // followed, a `Reject(Follow)` embedding their follow's URI toward each
    // follower. An edge stored without a URI has nothing to retract by
    // reference and is only deleted, as before.
    let mut retractions: Vec<job::QueuedDelivery> = Vec::new();
    for edge in &following {
        if let (Some(edge_uri), Some(target_uri)) = (&edge.edge_uri, &edge.other_uri) {
            let follow_activity = activity::follow_as_sent(
                edge_uri,
                &state.config.domain,
                &actor.username,
                target_uri,
            );
            retractions.push(job::QueuedDelivery {
                signer_account_id: actor.id,
                inbox_url: edge.other_inbox_url.clone(),
                activity: activity::undo(&state.config.domain, &actor.username, follow_activity),
            });
        }
    }
    for edge in &followers {
        if let (Some(edge_uri), Some(follower_uri)) = (&edge.edge_uri, &edge.other_uri) {
            let their_follow = activity::follow_as_received(
                edge_uri,
                follower_uri,
                &state.config.domain,
                &actor.username,
            );
            retractions.push(job::QueuedDelivery {
                signer_account_id: actor.id,
                inbox_url: edge.other_inbox_url.clone(),
                activity: activity::reject_follow(
                    &state.config.domain,
                    &actor.username,
                    id::next(),
                    their_follow,
                ),
            });
        }
    }

    let outgoing_targets: Vec<i64> = following.iter().map(|e| e.other_account_id).collect();
    let incoming_sources: Vec<i64> = followers.iter().map(|e| e.other_account_id).collect();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    job::enqueue_batch_tx(&mut *tx, &retractions).await?;
    follow::delete_out_many(&mut *tx, actor.id, &outgoing_targets).await?;
    follow::delete_in_many(&mut *tx, actor.id, &incoming_sources).await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    tracing::info!(
        account = %actor.username,
        domain = %job.domain,
        severed = following.len() + followers.len(),
        "domain block severed relationships"
    );
    Ok(())
}

async fn complete_severance(state: &AppState, id: i64) {
    if let Err(error) = domain_severance_job::complete(&state.pool, id).await {
        tracing::error!(%error, "failed to remove a settled domain severance job");
    }
}

/// Runs the severance loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("domain severance worker started");
        loop {
            if run_due(&state).await == 0
                && !crate::workers::pause(&state, SEVERANCE_IDLE_POLL).await
            {
                return;
            }
        }
    })
}
