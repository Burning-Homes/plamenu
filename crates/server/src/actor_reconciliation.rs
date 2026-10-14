//! Revalidate presentation text after an authenticated self-actor Update.
//!
//! Inline actor replacement remains authoritative for delivery and identity.
//! A stale queued Update can carry old text even though its origin already
//! publishes a newer profile. Reconciliation reads that exact actor URI and
//! changes only sanitized name/bio, with a database row-version comparison.
//! It never replaces keys, handles, URLs, suspension or authorization state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use plamenu_ap::actor::RemoteActor;
use plamenu_db::account::Account;
use plamenu_db::{DbError, account};

use crate::AppState;

const MAX_TASKS: usize = 256;
const MAX_ACTOR_URI_BYTES: usize = 8192;
const FETCH_SPACING: Duration = Duration::from_secs(1);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Coordinator {
    generations: Mutex<HashMap<i64, u64>>,
}

impl Coordinator {
    fn admit(self: &Arc<Self>, account_id: i64) -> Option<TaskGuard> {
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(generation) = generations.get_mut(&account_id) {
            *generation = generation.wrapping_add(1);
            return None;
        }
        if generations.len() >= MAX_TASKS {
            return None;
        }
        generations.insert(account_id, 1);
        Some(TaskGuard {
            coordinator: self.clone(),
            account_id,
            active: true,
        })
    }

    fn generation(&self, account_id: i64) -> Option<u64> {
        self.generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&account_id)
            .copied()
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

struct TaskGuard {
    coordinator: Arc<Coordinator>,
    account_id: i64,
    active: bool,
}

impl TaskGuard {
    // Removal and clean-generation comparison share one critical section:
    // an Update arriving after removal starts a new task instead of being lost.
    fn finish_if_clean(&mut self, generation: u64) -> bool {
        let mut generations = self
            .coordinator
            .generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if generations.get(&self.account_id) != Some(&generation) {
            return false;
        }
        generations.remove(&self.account_id);
        self.active = false;
        true
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.active {
            self.coordinator
                .generations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.account_id);
        }
    }
}

/// Only called after the inbox authenticated a self-Update and committed its
/// validated inline actor. No work is admitted for rejected/foreign Updates.
pub(crate) fn schedule(state: &AppState, account: &Account) {
    if account.has_local_account_on(&state.config.domain) || account.suspended() {
        return;
    }
    let Some(uri) = account.uri.as_deref() else {
        return;
    };
    if uri.len() > MAX_ACTOR_URI_BYTES || !plamenu_federation::is_federation_url(uri) {
        return;
    }
    let Some(guard) = state.actor_reconciliation.admit(account.id) else {
        return;
    };
    let state = state.clone();
    let uri = uri.to_owned();
    tokio::spawn(async move { run(state, uri, guard).await });
}

async fn run(state: AppState, uri: String, mut guard: TaskGuard) {
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => return,
            () = tokio::time::sleep(FETCH_SPACING) => {}
        }
        let Some(generation) = guard.coordinator.generation(guard.account_id) else {
            return;
        };
        // Origin fetches, local controller parsing and policy checks are all
        // outside the inbox path and bounded by one timeout per generation.
        tokio::select! {
            () = state.shutdown.cancelled() => return,
            _ = tokio::time::timeout(FETCH_TIMEOUT, reconcile(&state, &uri, guard.account_id, generation)) => {}
        }
        if guard.finish_if_clean(generation) {
            return;
        }
        // A real subsequent Update remains dirty and gets a current-origin
        // pass. There is no freshness-window drop; consecutive passes are
        // spaced so a signed flood cannot trigger an unbounded fetch loop.
    }
}

async fn reconcile(
    state: &AppState,
    uri: &str,
    account_id: i64,
    generation: u64,
) -> Result<(), DbError> {
    let Some(before) = account::find_by_id(&state.pool, account_id).await? else {
        return Ok(());
    };
    if before.uri.as_deref() != Some(uri)
        || before.suspended()
        || before.has_local_account_on(&state.config.domain)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await?
    {
        return Ok(());
    }
    let Some(revision) = account::remote_profile_revision(&state.pool, account_id, uri).await?
    else {
        return Ok(());
    };
    // This auxiliary signed read must not disclose credentials to a different
    // origin. The production override enforces origin, mutable domain policy,
    // SSRF, admission, redirects and body bounds before each request.
    let Ok(fetched) = state.federation.fetch_activitypub_same_origin(uri).await else {
        return Ok(());
    };
    let Some(document) = fetched.document else {
        return Ok(());
    };
    if !inline_classic_claims_match(&document, uri) {
        return Ok(());
    }
    let Ok(actor) = serde_json::from_value::<RemoteActor>(document) else {
        return Ok(());
    };
    if actor.id != uri
        || !matches!(
            actor.kind.as_str(),
            "Person" | "Service" | "Application" | "Group" | "Organization"
        )
        || !plamenu_federation::is_federation_url(&actor.inbox)
        || actor.public_key.owner != uri
        || actor.suspended
    {
        return Ok(());
    }
    // Presentation does not rotate or refresh authentication keys. Parse the
    // bounded inline owner/controller claims, rejecting incomplete external
    // references instead of making additional remote requests for them.
    let Ok(methods) = actor.verification_methods() else {
        return Ok(());
    };
    if methods.is_empty()
        || methods.iter().any(|method| {
            method.controller_uri != uri
                || !own_key_uri(&method.key_uri, uri)
                || method.revoked
                || method.algorithm.is_none()
                || method.public_key.is_none()
                || method
                    .expires_at
                    .is_some_and(|expires| expires <= time::OffsetDateTime::now_utc())
        })
    {
        return Ok(());
    }
    let display_name = plamenu_ap::text::sanitize_remote_plain(actor.name.as_deref().unwrap_or(""));
    let note = crate::remote::profile_summary_html(&actor);
    // Every intervening account write rejects this optional correction. In
    // particular, never retry with a new xmin: identical-text selfUpdates are
    // indistinguishable from unrelated churn before their generation admission.
    if state.actor_reconciliation.generation(account_id) != Some(generation)
        || !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await?
    {
        return Ok(());
    }
    account::reconcile_remote_profile_text(
        &state.pool,
        account_id,
        uri,
        &revision,
        &display_name,
        &note,
    )
    .await?;
    Ok(())
}

fn own_key_uri(key_uri: &str, actor_uri: &str) -> bool {
    key_uri == actor_uri
        || key_uri
            .strip_prefix(actor_uri)
            .is_some_and(|suffix| suffix.starts_with('#') || suffix.starts_with('/'))
}

// The existing classic-key parser may ignore invalid entries when another
// valid FEP key survives. Optional text reconciliation instead fails closed
// for every declared classic entry, including mixed arrays and references.
fn inline_classic_claims_match(document: &serde_json::Value, uri: &str) -> bool {
    let matches = |key: &serde_json::Value| {
        key.get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| own_key_uri(id, uri))
            && key.get("owner").and_then(serde_json::Value::as_str) == Some(uri)
            && key
                .get("publicKeyPem")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|pem| !pem.is_empty())
    };
    match document.get("publicKey") {
        Some(key @ serde_json::Value::Object(_)) => matches(key),
        Some(serde_json::Value::Array(keys)) => {
            !keys.is_empty() && keys.len() <= 10 && keys.iter().all(matches)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_and_global_state_bound() {
        let coordinator = Arc::new(Coordinator::default());
        let mut first = coordinator.admit(1).unwrap();
        let initial = coordinator.generation(1).unwrap();
        for _ in 0..100 {
            assert!(coordinator.admit(1).is_none());
        }
        assert_eq!(coordinator.active_count(), 1);
        assert!(!first.finish_if_clean(initial));
        let generation = coordinator.generation(1).unwrap();
        assert!(first.finish_if_clean(generation));
        let mut guards: Vec<_> = (1..=MAX_TASKS)
            .map(|id| coordinator.admit(i64::try_from(id).unwrap()).unwrap())
            .collect();
        assert!(coordinator.admit(999_999).is_none());
        assert_eq!(coordinator.active_count(), MAX_TASKS);
        guards.pop();
        assert!(coordinator.admit(999_999).is_some());
        drop(guards);
        assert_eq!(coordinator.active_count(), 0);
    }

    #[test]
    fn completed_guard_cannot_remove_a_later_task() {
        let coordinator = Arc::new(Coordinator::default());
        let mut first = coordinator.admit(1).unwrap();
        assert!(first.finish_if_clean(1));
        let second = coordinator.admit(1).unwrap();
        drop(first);
        assert_eq!(coordinator.active_count(), 1);
        drop(second);
        assert_eq!(coordinator.active_count(), 0);
    }
}
