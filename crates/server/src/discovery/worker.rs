//! Supervised, durable discovery acquisition. Public directories supply hints;
//! canonical actors supply independently verified profile evidence.
use super::transport::{self, Budget};
use crate::AppState;
use futures_util::{StreamExt, stream};
use plamenu_db::discovery_store::{self as db, Lease, Operation, VerifiedSource};
use plamenu_federation::{DiscoveryFormat, FederationError};
use std::time::Duration;
use time::OffsetDateTime;

async fn acquire(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    match lease.operation.as_str() {
        "refresh_presentation" => {
            super::presentation::server_icon(state, lease).await?;
            db::finish_job(&state.pool, lease)
                .await
                .map_err(|e| FederationError::Admission(e.to_string()))?;
            Ok(())
        }
        "inspect_source"
            if super::directory::is_endpoint(&lease.target_uri)
                || super::communities::is_endpoint(&lease.target_uri)
                || super::videos::is_endpoint(&lease.target_uri) =>
        {
            super::videos::inspect(state, lease).await
        }
        "inspect_source" => inspect(state, lease).await,
        "list_peers" => super::peers::fetch_page(state, lease).await,
        "list_directory" => super::directory::fetch_page(state, lease).await,
        "list_communities" => super::communities::fetch_page(state, lease).await,
        "sample_outbox" => super::outbox::fetch(state, lease).await,
        "refresh_entry" | "revalidate_entry" => {
            let known = db::entry(&state.pool, &lease.target_uri)
                .await
                .map_err(|e| FederationError::Admission(e.to_string()))?;
            if known.as_ref().is_some_and(|e| e.kind == "video")
                || lease.cursor.as_deref() == Some("video")
                || (known.is_none()
                    && db::source(&state.pool, lease.source_id)
                        .await
                        .map_err(|e| FederationError::Admission(e.to_string()))?
                        .is_some_and(|s| s.capabilities.iter().any(|c| c == "video_catalogue")))
            {
                super::videos::fetch(state, lease).await
            } else if known.as_ref().is_some_and(|e| e.kind == "community")
                || lease.cursor.as_deref() == Some("community")
                || (known.is_none()
                    && db::source(&state.pool, lease.source_id)
                        .await
                        .map_err(|e| FederationError::Admission(e.to_string()))?
                        .is_some_and(|s| s.capabilities.iter().any(|c| c == "community_directory")))
            {
                super::communities::fetch(state, lease).await
            } else if known.as_ref().is_some_and(|e| e.kind == "post")
                || lease
                    .cursor
                    .as_deref()
                    .is_some_and(|c| c.starts_with("https://") || c.starts_with("http://"))
            {
                super::posts::fetch(state, lease, known.as_ref()).await
            } else {
                super::profiles::fetch(state, lease).await
            }
        }
        _ => Err(FederationError::DiscoveryDeferred("adapter_unavailable")),
    }
}

async fn inspect(state: &AppState, lease: &Lease) -> Result<(), FederationError> {
    let response = transport::fetch(state, lease, &lease.target_uri, DiscoveryFormat::Json).await?;
    // Only reachability of an operator-supplied JSON endpoint is established.
    // Software labels and arbitrary advertised links confer no capability.
    if !response.document.is_object() {
        return Err(FederationError::InvalidActor(
            "source inspection needs a JSON object".into(),
        ));
    }
    if !(Budget { state, lease }).allowed().await? {
        return Err(FederationError::DiscoveryStale);
    }
    let now = OffsetDateTime::now_utc();
    db::finish_source(
        &state.pool,
        lease,
        &VerifiedSource {
            canonical_uri: &response.final_url,
            capabilities: &["source_inspection".into()],
            verified_at: now,
            valid_until: now + time::Duration::hours(24),
        },
    )
    .await
    .map_err(|e| FederationError::Admission(e.to_string()))?;
    Ok(())
}

async fn superseded(state: &AppState, lease: &Lease) {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !(Budget { state, lease }).allowed().await.unwrap_or(false) {
            return;
        }
    }
}

fn retry(error: &FederationError, lease: &Lease) -> (i32, &'static str, bool) {
    let jitter = i32::try_from(lease.lease_token.rem_euclid(31)).unwrap_or_default();
    match error {
        FederationError::RateLimited { retry_after_secs } => (
            i32::try_from(retry_after_secs.unwrap_or(60).clamp(1, 604_769)).unwrap_or(604_769)
                + jitter,
            "rate_limited",
            true,
        ),
        FederationError::Status(401 | 403) => (86_400 + jitter, "access_denied", false),
        FederationError::Status(404 | 405 | 410 | 501) => {
            (86_400 + jitter, "endpoint_unsupported", false)
        }
        FederationError::InvalidActor(_) => (86_400 + jitter, "invalid_document", false),
        FederationError::DocumentTooLarge(_) => (86_400 + jitter, "body_too_large", false),
        FederationError::InvalidUrl(_) | FederationError::PrivateAddress(_) => {
            (86_400 + jitter, "blocked_destination", false)
        }
        FederationError::Stalled(_) => (900 + jitter, "deadline", false),
        _ => (
            (900 * (1_i32 << (lease.attempts - 1).clamp(0, 6))).min(86_400) + jitter,
            "fetch_failed",
            false,
        ),
    }
}

async fn run_one(state: &AppState, lease: Lease) {
    // Dropping a fetch on shutdown/pause closes its body stream. Its durable
    // reservation remains charged and holds its slot through the hard deadline.
    let result = tokio::select! {
        biased;
        ()=state.shutdown.cancelled()=>return,
        ()=superseded(state,&lease)=>Err(FederationError::DiscoveryStale),
        result=tokio::time::timeout(Duration::from_secs(30),acquire(state,&lease))=>result.unwrap_or_else(|_|Err(FederationError::Stalled("discovery job deadline".into()))),
    };
    let outcome = match result {
        Ok(()) => return,
        Err(FederationError::DiscoveryStale) => {
            // A settings change already fenced the lease; a live instance
            // block instead needs a bounded deferral to release it.
            db::defer_job(&state.pool, &lease, 300, "policy_changed").await
        }
        Err(FederationError::DiscoveryDeferred(reason)) => {
            let delay = if reason == "budget_exhausted" {
                300
            } else {
                30
            };
            db::defer_job(&state.pool, &lease, delay, reason).await
        }
        Err(error) => {
            let (delay, reason, origin) = retry(&error, &lease);
            tracing::warn!(source=lease.source_id, operation=%lease.operation, reason,
                error=%super::profiles::text(&error.to_string(), 512),
                "discovery remote acquisition failed");
            if lease.operation == "list_peers" && !origin {
                db::fail_peer_page(&state.pool, &lease, reason).await
            } else if ["refresh_entry", "revalidate_entry", "sample_outbox"]
                .contains(&lease.operation.as_str())
                && !origin
            {
                db::retry_job(&state.pool, &lease, lease.cursor.as_deref(), delay, reason).await
            } else {
                db::fail_job(&state.pool, &lease, delay, reason, origin).await
            }
        }
    };
    if let Err(error) = outcome {
        tracing::warn!(%error,job=lease.id,"discovery result persistence failed");
    }
}

pub async fn run_due(state: &AppState) -> Result<usize, plamenu_db::DbError> {
    if state.shutdown.is_cancelled() {
        return Ok(0);
    }
    db::prune_jobs(&state.pool).await?;
    db::schedule_presentation(&state.pool).await?;
    db::schedule_peer_exploration(&state.pool).await?;
    db::schedule_directory_maintenance(&state.pool).await?;
    db::schedule_outboxes(&state.pool).await?;
    let jobs = db::claim_due_for(
        &state.pool,
        &format!("discovery-{}", std::process::id()),
        &[
            Operation::InspectSource,
            Operation::ListPeers,
            Operation::ListDirectory,
            Operation::ListCommunities,
            Operation::SampleOutbox,
            Operation::RefreshEntry,
            Operation::RevalidateEntry,
            Operation::RefreshPresentation,
        ],
    )
    .await?;
    let count = jobs.len();
    stream::iter(jobs)
        .for_each_concurrent(None, |lease| run_one(state, lease))
        .await;
    Ok(count)
}

#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = run_due(&state).await {
                tracing::warn!(%error,"discovery acquisition failed");
            }
            if !crate::workers::pause(&state, Duration::from_secs(5)).await {
                return;
            }
        }
    })
}
