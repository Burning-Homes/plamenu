//! Server self-destruct — Mastodon's `tootctl self-destruct` +
//! `Scheduler::SelfDestructScheduler`: a clean exit from the fediverse.
//!
//! Once the operator arms the mode (`plamenu self-destruct`, persisted in
//! `instance_settings.self_destruct_initiated_at`), two things happen:
//!
//! - the [`gate`] middleware serves `410 Gone` for every request except the flows a winding-down
//!   server still owes its users — signing in and taking their data export (the CSV downloads and
//!   the full-data archive alike) — plus the assets those pages need. The blanket 410 also covers
//!   the `ActivityPub` surface on purpose: remotes
//!   fetching an actor get the strongest possible "gone" signal, and inbox POSTs are refused so
//!   peers stop delivering.
//! - the [`spawn`]ed worker broadcasts a `Delete(Actor)` for every local account to every inbox
//!   this server has ever known (not just followers). Each account's whole fan-out is queued and the
//!   account suspended in one transaction ([`account::broadcast_self_destruct`]), so a crash
//!   mid-broadcast retries the account cleanly rather than re-queuing a duplicate prefix, and each
//!   pass reserves delivery-queue capacity per account so it cannot overshoot the backpressure cap.
//!   The regular delivery worker drains the queue with its usual retries.
//!
//! Like Mastodon, no local data is erased: the database can be dropped
//! wholesale once [`progress`] reports every notice delivered. The mode is
//! one-way — the state mismatch after remotes drop our accounts makes the
//! server unusable for anything but the wind-down.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use maud::html;
use plamenu_db::account::{self, Account};
use plamenu_db::{instance_settings, job};
use serde_json::json;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::error::ApiError;

/// Accounts whose deletion notices are queued per worker pass (Mastodon's
/// `MAX_ACCOUNT_DELETIONS_PER_JOB`).
const ACCOUNTS_PER_PASS: i64 = 50;

/// Queue-depth backpressure: no new account is broadcast while this many
/// deliveries are still pending (Mastodon's `MAX_ENQUEUED`).
const MAX_ENQUEUED: u64 = 10_000;

/// How often the worker re-checks the flag and the queue.
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

/// Middleware over the whole router: `410 Gone` for everything while in
/// self-destruct mode, except sign-in, password reset, the data export (CSV
/// downloads and the full archive), and the assets/health checks those need
/// (Mastodon's `check_self_destruct!` skip list).
pub async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // The cached settings row is already consulted per-request by the rate
    // limiter, so this adds no query in the steady state.
    let self_destructing = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings.is_self_destructing(),
        Err(error) => {
            tracing::error!(error = %crate::error::ErrorChain(&error), "self-destruct gate could not read settings");
            false
        }
    };
    if !self_destructing || exempt(request.uri().path()) {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if path.starts_with("/api/") || path.starts_with("/oauth/") {
        // Mastodon's JSON rendering of the 410.
        return (StatusCode::GONE, axum::Json(json!({ "error": "Gone" }))).into_response();
    }
    (StatusCode::GONE, Html(gone_page().into_string())).into_response()
}

/// The wind-down allowlist: users can still sign in and take their data
/// export with them; ops keep both health probes (`/health` liveness and
/// `/ready` readiness — the container health check gates on the latter, and the
/// datastore stays up to serve the export, so it honestly reports ready).
///
/// The export the [`gone_page`] promises is not only the CSV downloads under
/// `/settings/export*`: the export page also offers the full-data archive, so
/// its two routes stay open — requesting a build
/// (`POST /web/settings/archive`) and downloading a finished ZIP
/// (`GET /settings/archive/{id}/download`). Each has exactly one method, and
/// the match is scoped tightly (the archive family is only exempt on its
/// `/download` leaf), so every other settings surface — imports, cleanup,
/// profile edits, and the rest — stays gated. The archive worker keeps running
/// during wind-down, so a build requested now is finished and downloadable
/// before teardown.
fn exempt(path: &str) -> bool {
    path == "/health"
        || path == "/ready"
        || path == "/logout"
        || path == "/login"
        || path.starts_with("/login/")
        || path.starts_with("/auth/")
        || path == "/settings/export"
        || path.starts_with("/settings/export/")
        || path == "/web/settings/archive"
        || (path.starts_with("/settings/archive/") && path.ends_with("/download"))
        || path.starts_with("/assets/")
        || path.starts_with("/pwa/")
        || path == "/custom.css"
        || path == "/favicon.ico"
        || path == "/apple-touch-icon.png"
        || path == "/manifest.webmanifest"
        || path == "/sw.js"
        || path == "/offline"
}

/// The HTML shown for gated web requests (Mastodon's `errors/self_destruct`).
fn gone_page() -> maud::Markup {
    crate::web::layout::focused_shell(
        "This server is shutting down",
        &html! {
            section.card {
                h1 { "This server is permanently going offline" }
                p {
                    "The operator has begun shutting this server down for good. "
                    "New posts and interactions are no longer accepted."
                }
                p {
                    "You can still " a href="/login" { "sign in" }
                    " to download an archive of your data from the "
                    a href="/settings/export" { "export page" } "."
                }
            }
        },
    )
}

/// A snapshot of the wind-down, for the CLI's progress report.
pub struct Progress {
    /// Local accounts whose deletion notices have not been queued yet.
    pub pending_accounts: u64,
    /// Deletion deliveries still sitting in the queue (including retries).
    pub pending_deliveries: u64,
}

/// Reads the current wind-down state.
pub async fn progress(state: &AppState) -> Result<Progress, ApiError> {
    Ok(Progress {
        pending_accounts: account::count_self_destruct_pending(&state.pool).await?,
        pending_deliveries: job::pending_count(&state.pool).await?,
    })
}

/// One worker pass: queue deletion notices for up to [`ACCOUNTS_PER_PASS`]
/// accounts, unless the delivery queue is already saturated. Returns how many
/// accounts were broadcast (0 = nothing left to do, or backpressure).
pub async fn run_once(state: &AppState) -> Result<usize, ApiError> {
    let mut pending = job::pending_count(&state.pool).await?;
    // Backpressure: don't pile onto an already-saturated queue. `>=` (not `>`)
    // so exactly [`MAX_ENQUEUED`] already reads as full (finding #42).
    if pending >= MAX_ENQUEUED {
        return Ok(0);
    }
    let batch = account::self_destruct_pending(&state.pool, ACCOUNTS_PER_PASS).await?;
    if batch.is_empty() {
        return Ok(0);
    }
    // One audience snapshot for the whole batch: with the gate up nothing
    // can add remote accounts mid-pass.
    let inboxes = account::known_remote_inboxes(&state.pool).await?;
    let chunk = inboxes.len() as u64;
    let mut broadcast = 0;
    for local_account in batch {
        // Reserve capacity for this account's whole chunk before starting it, so
        // a single pass cannot blow past MAX_ENQUEUED by ACCOUNTS_PER_PASS ×
        // inboxes at once (finding #42). An empty queue always makes progress,
        // even for one audience larger than the soft cap.
        if !chunk_fits(pending, chunk) {
            break;
        }
        broadcast_deletion(state, &local_account, &inboxes).await?;
        pending += chunk;
        broadcast += 1;
    }
    Ok(broadcast)
}

/// Whether an account's `chunk` deliveries may be queued now, given `pending`
/// already in the queue: it fits under [`MAX_ENQUEUED`], or the queue is empty so
/// the wind-down must still make progress even when one audience exceeds the
/// soft cap.
fn chunk_fits(pending: u64, chunk: u64) -> bool {
    pending == 0 || pending + chunk <= MAX_ENQUEUED
}

/// Atomically queues this account's `Delete(Actor)` to every known inbox and
/// marks it suspended (finding #42): the fan-out and the suspension commit
/// together, so a crash mid-broadcast leaves the account fully pending with no
/// partial prefix to re-enqueue as duplicates, and one batched insert replaces
/// the former per-inbox round trips. Once committed the actor answers `410 Gone`
/// (via the gate and the tombstone alike) while the queued notices go out signed
/// with its key, which suspension keeps intact.
async fn broadcast_deletion(
    state: &AppState,
    local_account: &Account,
    inboxes: &[String],
) -> Result<(), ApiError> {
    let activity =
        plamenu_ap::activity::delete_actor(&state.config.domain, &local_account.username);
    account::broadcast_self_destruct(&state.pool, local_account.id, inboxes, &activity).await?;
    tracing::info!(
        user = %local_account.username,
        inboxes = inboxes.len(),
        "self-destruct: deletion notices queued"
    );
    Ok(())
}

/// Runs the self-destruct broadcaster until the process exits. Idle (one
/// cheap flag read per tick) until the mode is armed — the CLI writes the
/// flag to the database, so no restart is needed.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let armed = match instance_settings::get(&state.pool).await {
                Ok(settings) => settings.is_self_destructing(),
                Err(error) => {
                    tracing::error!(error = %crate::error::ErrorChain(&error), "self-destruct worker could not read settings");
                    false
                }
            };
            if armed {
                match run_once(&state).await {
                    // Keep draining without the tick while batches are full.
                    Ok(n) if i64::try_from(n) == Ok(ACCOUNTS_PER_PASS) => continue,
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(error = %error.chain(), "self-destruct pass failed");
                    }
                }
            }
            if !crate::workers::pause(&state, TICK).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_ENQUEUED, chunk_fits};

    #[test]
    fn chunk_fits_reserves_capacity_and_always_progresses_when_empty() {
        // An empty queue always admits a chunk — even one larger than the whole
        // cap — so the wind-down cannot stall on a single huge audience.
        assert!(chunk_fits(0, MAX_ENQUEUED * 2));
        // Exactly filling the cap is allowed…
        assert!(chunk_fits(MAX_ENQUEUED - 5, 5));
        // …one delivery past it is not: the pass stops before overshooting.
        assert!(!chunk_fits(MAX_ENQUEUED - 5, 6));
        // At or above the cap nothing more fits (the `>=` backpressure boundary).
        assert!(!chunk_fits(MAX_ENQUEUED, 1));
        assert!(!chunk_fits(MAX_ENQUEUED + 1, 1));
    }
}
