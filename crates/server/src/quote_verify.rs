//! Background quote re-verification — Mastodon's
//! `RefetchAndVerifyQuoteWorker`, plus one extra step suited to relay-fed
//! instances: the quoting post itself is re-fetched from its origin. Mastodon
//! only delivers the `quoteAuthorization` stamp of a freshly accepted quote
//! inside a later Update, which a relay may never forward — but the origin's
//! post JSON carries the stamp from the moment the handshake completes, so
//! re-fetching it recovers the authorization regardless of delivery gaps.

use std::time::Duration;

use plamenu_ap::activity::id_of;
use plamenu_db::{account, quote, quote_verify_job};
use tokio::task::JoinHandle;

use crate::error::ApiError;
use crate::{AppState, ingest};

const BATCH_SIZE: i64 = 4;
const IDLE_POLL: Duration = Duration::from_secs(5);
/// Verification attempts before a quote is left `pending` for good (a
/// stamp-less legacy quote then renders as a pending placeholder, like on
/// Mastodon).
const MAX_ATTEMPTS: i32 = 5;
/// Backoff base; attempt `n` waits `RETRY_BASE * 2^n` (60s → ~16min).
const RETRY_BASE: Duration = Duration::from_mins(1);

fn retry_delay(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.clamp(0, 6)).unwrap_or(0);
    RETRY_BASE.saturating_mul(1 << shift)
}

/// What became of a claimed job, so the worker knows whether to remove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The verification settled or exhausted its retries; drop the job row.
    Done,
    /// The verification failed transiently and was re-queued with backoff; the
    /// (leased) row was updated in place and must stay.
    Retried,
}

/// Re-queues the quote unless its attempt budget ran out.
async fn reschedule(
    state: &AppState,
    job: quote_verify_job::ClaimedJob,
) -> Result<Outcome, ApiError> {
    let next = job.attempts + 1;
    if next < MAX_ATTEMPTS {
        quote_verify_job::reschedule(&state.pool, job.quote_id, next, retry_delay(job.attempts))
            .await?;
        Ok(Outcome::Retried)
    } else {
        tracing::debug!(
            quote = job.quote_id,
            "giving up quote verification; leaving it pending"
        );
        Ok(Outcome::Done)
    }
}

/// One verification attempt for a claimed job.
async fn process(state: &AppState, job: quote_verify_job::ClaimedJob) -> Result<Outcome, ApiError> {
    let Some(row) = quote::find_by_id(&state.pool, job.quote_id).await? else {
        return Ok(Outcome::Done);
    };
    if row.state != "pending" {
        return Ok(Outcome::Done);
    }
    let Some(sender) = account::find_by_id(&state.pool, row.account_id).await? else {
        return Ok(Outcome::Done);
    };
    // Local quotes settle through the remote author's Accept/Reject, not here.
    if sender.is_local() {
        return Ok(Outcome::Done);
    }

    // Refresh the stamp and quoted URI from the origin's live copy.
    let (quoted_uri, stamp) = match state.federation.fetch_object(&row.status_uri).await {
        Ok(json) if id_of(&json) == Some(row.status_uri.as_str()) => {
            let fields = ingest::quote_fields(&json);
            if fields.legacy != row.legacy {
                quote::set_legacy(&state.pool, row.id, fields.legacy).await?;
            }
            (
                fields
                    .quoted_uri
                    .map(str::to_owned)
                    .or_else(|| row.quoted_uri.clone()),
                fields
                    .authorization
                    .map(str::to_owned)
                    .or_else(|| row.approval_uri.clone()),
            )
        }
        // The quoting post is gone at its origin; the Delete will clean up.
        Err(plamenu_federation::FederationError::Status(404 | 410)) => return Ok(Outcome::Done),
        // Transient origin trouble (or an id mismatch): retry with what we
        // already know.
        _ => (row.quoted_uri.clone(), row.approval_uri.clone()),
    };
    let Some(quoted_uri) = quoted_uri else {
        // Nothing to verify against and the origin doesn't say either.
        return reschedule(state, job).await;
    };

    let outcome = ingest::evaluate_inbound_quote(
        state,
        &sender,
        &row.status_uri,
        &quoted_uri,
        stamp.as_deref(),
        0,
    )
    .await?;
    quote::set_quoted_uri(&state.pool, row.id, &quoted_uri).await?;
    let still_pending =
        ingest::apply_quote_outcome(state, &row, &outcome, stamp.as_deref()).await?;
    if still_pending {
        reschedule(state, job).await
    } else {
        tracing::info!(
            quote = row.id,
            state = outcome.state,
            "quote verification settled"
        );
        Ok(Outcome::Done)
    }
}

/// Claims and runs one batch of due verifications; returns how many were
/// claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match quote_verify_job::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim quote verify jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    for job in jobs {
        match process(state, job).await {
            // Settled or retries exhausted: remove the leased row.
            Ok(Outcome::Done) => {
                if let Err(error) = quote_verify_job::complete(&state.pool, job.id).await {
                    tracing::error!(%error, quote = job.quote_id, "failed to complete quote verify job");
                }
            }
            // Re-queued with backoff; the row stays.
            Ok(Outcome::Retried) => {}
            // An unexpected failure: leave the leased row so its lease expires
            // and the verification is retried rather than lost.
            Err(error) => {
                tracing::warn!(error = %error.chain(), quote = job.quote_id, "quote verification failed");
            }
        }
    }
    claimed
}

/// Runs the verification loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("quote verify worker started");
        loop {
            if run_due(&state).await == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}
