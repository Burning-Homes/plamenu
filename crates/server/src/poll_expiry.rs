//! The poll-expiry sweeper: each due poll is claimed exactly once, then —
//! like Mastodon's `PollExpirationNotifyWorker` — local polls fan out their
//! final `Update(Question)` and notify their author, and local voters of
//! any poll get a `poll` notification. `run_due` does one batch (and is
//! what tests call); `spawn` wraps it in the long-running server task.

use plamenu_db::{account, notification, poll, status};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;

const BATCH_SIZE: i64 = 20;
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Claims and processes one batch of due poll expirations; returns how many
/// polls were claimed (0 = nothing is currently due).
pub async fn run_due(state: &AppState) -> u64 {
    let due = match poll::claim_due_expirations(&state.pool, BATCH_SIZE).await {
        Ok(due) => due,
        Err(error) => {
            tracing::error!(%error, "failed to claim due poll expirations");
            return 0;
        }
    };
    let claimed = due.len() as u64;
    for expired in due {
        // The poll stays claimed on failure: expiry work is best-effort
        // side effects (notifications), not something to re-run blindly.
        if let Err(error) = process(state, &expired).await {
            tracing::warn!(error = %error.chain(), poll = expired.id, "poll expiry processing failed");
        }
    }
    claimed
}

async fn process(state: &AppState, expired: &poll::Poll) -> Result<(), ApiError> {
    let Some(item) = status::find_by_id(&state.pool, expired.status_id).await? else {
        return Ok(());
    };
    let Some(author) = account::find_by_id(&state.pool, expired.account_id).await? else {
        return Ok(());
    };
    if author.is_local() {
        // Remote servers get the final tallies (now marked `closed`)…
        crate::polls::distribute_poll_update(state, &item, expired).await?;
        // …and the author hears their own poll ended (the one
        // self-notification Mastodon allows).
        notification::create(&state.pool, author.id, author.id, "poll", Some(item.id)).await?;
    }
    for voter in poll::local_voters_of(&state.pool, expired.id).await? {
        notification::create(&state.pool, voter, author.id, "poll", Some(item.id)).await?;
    }
    tracing::info!(poll = expired.id, "poll expired and processed");
    Ok(())
}

/// Runs the expiry sweep until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("poll expiry sweeper started");
        loop {
            // Drain everything currently due, then sleep out the interval.
            while run_due(&state).await > 0 {}
            if !crate::workers::pause(&state, SWEEP_INTERVAL).await {
                return;
            }
        }
    })
}
