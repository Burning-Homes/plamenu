//! Bounded automatic recovery of channel attribution for Videos imported
//! before Plamenu stored their PeerTube-style Group identity. One claim is
//! processed at a time, with durable leases and backoff; no operator action
//! or companion service is required.

use std::time::Duration;

use plamenu_ap::activity::attributed_to_ids;
use plamenu_db::{account, status, video};
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;
use crate::ingest;

const START_DELAY: Duration = Duration::from_secs(30);
const BETWEEN_FETCHES: Duration = Duration::from_secs(15);
const IDLE_POLL: Duration = Duration::from_mins(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Settled,
    Retry,
}

fn retry_delay(attempts: i32) -> Duration {
    Duration::from_mins(1).saturating_mul(1 << attempts.clamp(0, 10))
}

async fn process(
    state: &AppState,
    job: &video::RemoteChannelBackfill,
) -> Result<Outcome, ApiError> {
    let Some(stored) = status::find_by_id(&state.pool, job.status_id).await? else {
        return Ok(Outcome::Settled);
    };
    let Some(author) = account::find_by_id(&state.pool, job.account_id).await? else {
        return Ok(Outcome::Settled);
    };
    let Some(author_uri) = author.uri.as_deref() else {
        return Ok(Outcome::Settled);
    };
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, &job.uri)
        .await?
    {
        return Ok(Outcome::Retry);
    }
    let object = match state.federation.fetch_object(&job.uri).await {
        Ok(object) => object,
        Err(plamenu_federation::FederationError::Status(404 | 410)) => {
            return Ok(Outcome::Settled);
        }
        Err(error) => {
            tracing::debug!(status_id = job.status_id, %error, "remote video channel backfill fetch deferred");
            return Ok(Outcome::Retry);
        }
    };
    // The canonical object must still be the Video attributed to the same
    // author. A changed object, redirected id or unrelated author is not a
    // trustworthy source for mutating this stored status's channel relation.
    if object.get("type").and_then(serde_json::Value::as_str) != Some("Video")
        || !attributed_to_ids(&object).any(|id| id == author_uri)
    {
        return Ok(Outcome::Settled);
    }
    if ingest::store_remote_video_channel(state, &author, &stored, &object).await? {
        Ok(Outcome::Settled)
    } else {
        Ok(Outcome::Retry)
    }
}

/// One leased network attempt. `true` means work was claimed; an idle worker
/// backs off its polling rather than repeatedly scanning the status index.
pub async fn run_once(state: &AppState) -> Result<bool, ApiError> {
    let Some(job) = video::claim_remote_channel_backfill(&state.pool).await? else {
        return Ok(false);
    };
    match process(state, &job).await {
        Ok(Outcome::Settled) => {
            video::complete_remote_channel_check(&state.pool, job.status_id).await?;
        }
        Ok(Outcome::Retry) => {
            video::retry_remote_channel_check(
                &state.pool,
                job.status_id,
                retry_delay(job.attempts),
            )
            .await?;
        }
        Err(error) => {
            tracing::warn!(status_id = job.status_id, %error, "remote video channel backfill attempt failed");
            video::retry_remote_channel_check(
                &state.pool,
                job.status_id,
                retry_delay(job.attempts),
            )
            .await?;
        }
    }
    Ok(true)
}

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("remote video channel backfill worker started");
        if !crate::workers::pause(&state, START_DELAY).await {
            return;
        }
        loop {
            let delay = match run_once(&state).await {
                Ok(true) => BETWEEN_FETCHES,
                Ok(false) => IDLE_POLL,
                Err(error) => {
                    tracing::warn!(%error, "remote video channel backfill claim failed");
                    Duration::from_mins(1)
                }
            };
            if !crate::workers::pause(&state, delay).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::retry_delay;
    use std::time::Duration;

    #[test]
    fn retries_are_bounded_after_repeated_origin_failures() {
        assert_eq!(retry_delay(0), Duration::from_mins(1));
        assert_eq!(retry_delay(3), Duration::from_mins(8));
        assert_eq!(retry_delay(30), Duration::from_mins(1024));
    }
}
