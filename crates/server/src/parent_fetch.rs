//! Chasing the parent of an orphaned reply in the background.
//!
//! A reply can reach us without its parent — the parent's origin was down when
//! it arrived, or the reply was forwarded by someone else, or the thread was
//! only ever partly delivered. The system stores such a reply with `in_reply_to_uri`
//! and no `in_reply_to_id`, and every view of it since has shown the "on
//! {host}" fallback link instead of a thread. Nothing was ever going back for
//! the parent.
//!
//! Rendering a feed is now that trigger: when a card is a reply whose parent
//! we never fetched, the parent's URI goes to this module, which fetches and
//! ingests it out of band. `adopt_orphan_replies` links the waiting
//! children as part of the ingest, so the *next* view of the same feed draws
//! the thread — this render is never delayed and never fails because of it.
//!
//! Feed rendering must not become a fetch amplifier, so:
//!
//! * **at most [`MAX_PER_PAGE`] parents per render** — a page of twenty replies
//!   to twenty unknown parents is not twenty requests;
//! * **at most [`MAX_CONCURRENT`] in flight across the whole server**, taken
//!   with `try_acquire`, so a busy instance drops the extra work rather than
//!   queueing an unbounded pile of it (a later view retries for free);
//! * **one attempt per URI at a time**, deduplicated in process;
//! * **the persistent retry budget decides whether to try at all** —
//!   `remote_fetch_failure` already backs off per resource and per host and
//!   abandons both for good after eight failures, which is what stops us
//!   knocking forever on a peer that has vanished.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::Semaphore;

use crate::AppState;
use crate::sync::RecoverableMutex as _;

/// Parents chased per rendered page.
const MAX_PER_PAGE: usize = 4;
/// Parent fetches in flight at once, server-wide.
const MAX_CONCURRENT: usize = 4;

/// One slot per URI being fetched right now.
static INFLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

static PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT)));

/// Marks a URI as being fetched; [`None`] when another render already is.
struct Guard(String);

impl Guard {
    fn begin(uri: &str) -> Option<Self> {
        // The guard's `Drop` locks `INFLIGHT` too, so it must never be built —
        // or, as `then_some`'s eagerly-evaluated argument, dropped — while this
        // statement still holds the lock. That froze the runtime once already
        // (media_worker, 2026-07-12).
        let inserted = INFLIGHT.lock_or_recover().insert(uri.to_owned());
        inserted.then(|| Self(uri.to_owned()))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        INFLIGHT.lock_or_recover().remove(&self.0);
    }
}

/// Fetches the parents named by `uris` out of band, best-effort and capped.
/// Returns immediately; the render never waits on any of this.
pub fn spawn_resolves(state: &AppState, uris: Vec<String>) {
    let mut seen = HashSet::new();
    for uri in uris {
        if !seen.insert(uri.clone()) {
            continue; // several replies to the same missing parent: one fetch
        }
        if seen.len() > MAX_PER_PAGE {
            break;
        }
        let state = state.clone();
        tokio::spawn(async move { resolve_one(&state, &uri).await });
    }
}

async fn resolve_one(state: &AppState, uri: &str) {
    let Ok(permit) = Arc::clone(&PERMITS).try_acquire_owned() else {
        return; // the server is already chasing as many parents as it will
    };
    let Some(_guard) = Guard::begin(uri) else {
        return; // another render is already on this one
    };
    // A URI whose budget is spent (or whose host's is) must not be knocked on
    // again. `fetch_object` checks this too, but checking here keeps a
    // black-holed thread from costing a task and a permit per page view.
    match plamenu_db::remote_fetch_failure::should_attempt(&state.pool, "resource", uri).await {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            tracing::debug!(error = %error, uri = %uri, "parent fetch budget check failed");
            return;
        }
    }
    match crate::ingest::resolve_or_fetch_status(state, uri).await {
        // The ingest adopts the replies that were waiting on it, so the
        // next render of the same feed draws them as a thread.
        Ok(Some(parent)) => {
            tracing::debug!(uri = %uri, parent = parent.id, "fetched an orphan's parent");
        }
        Ok(None) => tracing::debug!(uri = %uri, "orphan's parent did not resolve"),
        Err(error) => {
            tracing::debug!(error = %error.chain(), uri = %uri, "orphan parent fetch failed");
        }
    }
    drop(permit);
}

#[cfg(test)]
mod tests {
    use super::Guard;

    #[test]
    fn one_uri_is_chased_once_at_a_time() {
        let held = Guard::begin("https://remote.example/notes/1");
        assert!(held.is_some(), "uncontended");
        assert!(
            Guard::begin("https://remote.example/notes/1").is_none(),
            "a second render finds it already in flight"
        );
        drop(held);
        assert!(
            Guard::begin("https://remote.example/notes/1").is_some(),
            "the slot is released when the fetch finishes"
        );
    }
}
