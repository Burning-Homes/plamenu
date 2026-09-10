//! The delivery worker: drains the Postgres job queue through the federation
//! transport. `run_due` does one batch (and is what tests call); `spawn`
//! wraps it in the long-running server task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plamenu_db::account::Account;
use plamenu_db::reachability::HostReachability;
use plamenu_db::{DbError, PgListener, PgPool, account, job, reachability};
use plamenu_federation::FederationError;
use serde_json::Value;
use tokio::task::{JoinHandle, JoinSet};
use zeroize::Zeroizing;

use crate::AppState;
use crate::federation::Delivery;
use crate::sync::RecoverableMutex as _;

const BATCH_SIZE: i64 = 20;
const CONCURRENT_INBOXES: usize = 8;
const LISTENER_RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// How long a host must have been marked unreachable before low-value
/// deliveries to it start being skipped. Between the breaker opening and
/// this point everything is still attempted — the window only instruments.
const SKIP_UNREACHABLE_AFTER: time::Duration = time::Duration::days(3);
/// While skipping, one low-value delivery per host and interval still goes
/// out as a probe so recovery is noticed without waiting for a high-value
/// delivery.
const PROBE_INTERVAL_SECS: f64 = 6.0 * 3600.0;

/// How a delivery attempt failed, and whether the remote host can be blamed
/// for it (local errors — a vanished signing key, a DB hiccup — must never
/// poison the host's reachability record).
struct DeliveryFailure {
    class: FailureClass,
    remote: bool,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    /// 404/410: the inbox (or its owner) no longer exists. Terminal.
    Gone,
    /// The remote understood and refused (400/403/422/501); an identical
    /// retry cannot change the answer. Terminal.
    Permanent,
    /// 429 — retry, but no sooner than the remote asked.
    RateLimited { retry_after_secs: Option<u64> },
    /// Network errors, timeouts, 5xx, and everything else worth retrying
    /// (401 stays retryable: receiver-side key-fetch hiccups produce it).
    Transient,
}

impl DeliveryFailure {
    /// A failure reported by the remote transport. `hidden_service` marks an
    /// inbox reached through a Tor/I2P circuit.
    fn from_federation(error: &FederationError, hidden_service: bool) -> Self {
        let class = match error {
            FederationError::Status(404 | 410) => FailureClass::Gone,
            FederationError::Status(400 | 403 | 422 | 501) => FailureClass::Permanent,
            FederationError::RateLimited { retry_after_secs } => FailureClass::RateLimited {
                retry_after_secs: *retry_after_secs,
            },
            _ => FailureClass::Transient,
        };
        // `Http` covers connect/DNS/timeout failures — the remote
        // infrastructure, not us — so it counts against the host too.
        // Except through an overlay circuit: there a transport failure is
        // routinely OUR Tor/I2P leg (circuits fail far more often than the
        // service behind them), so only an actual HTTP answer is
        // remote-attributable — otherwise ordinary circuit churn marks every
        // onion host unreachable (plan §7.1).
        let remote = matches!(
            error,
            FederationError::Status(_) | FederationError::RateLimited { .. }
        ) || (!hidden_service && matches!(error, FederationError::Http(_)));
        Self {
            class,
            remote,
            message: error.to_string(),
        }
    }

    /// A failure in our own pipeline before/after the wire.
    fn local(error: impl std::fmt::Display) -> Self {
        Self {
            class: FailureClass::Transient,
            remote: false,
            message: error.to_string(),
        }
    }

    fn reachability_class(&self) -> &'static str {
        match self.class {
            FailureClass::Gone => reachability::CLASS_GONE,
            FailureClass::Permanent => reachability::CLASS_PERMANENT,
            FailureClass::RateLimited { .. } => reachability::CLASS_RATE_LIMITED,
            FailureClass::Transient => reachability::CLASS_TRANSIENT,
        }
    }
}

/// The two ways an attempt can succeed: the activity went out, or the job
/// was deliberately not sent (blocked domain, cancelled, breaker skip) —
/// only real deliveries feed the host's reachability record.
enum AttemptOutcome {
    Delivered,
    Skipped,
}

/// The lowercase HTTP host a delivery targets — the reachability key.
fn inbox_host(inbox_url: &str) -> Option<String> {
    let url: url::Url = inbox_url.parse().ok()?;
    url.host_str().map(str::to_lowercase)
}

/// Deliveries that must survive an open breaker: relationship changes and
/// retractions, plus posts addressed to specific actors (mentions and DMs)
/// rather than Public/followers fan-out. Boosts and reactions stay
/// low-value even though their `cc` names the original author.
fn is_high_value(activity: &Value) -> bool {
    const AS_PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    match activity["type"].as_str().unwrap_or_default() {
        "Follow" | "Accept" | "Reject" | "Undo" | "Block" | "Move" | "Delete" | "Flag" => true,
        "Create" | "Update" => ["to", "cc"].iter().any(|field| {
            let addressed = &activity[*field];
            let mut uris = match addressed {
                Value::String(uri) => vec![uri.as_str()],
                Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            uris.retain(|uri| *uri != AS_PUBLIC && !uri.ends_with("/followers"));
            !uris.is_empty()
        }),
        _ => false,
    }
}

/// Facts memoized across one claimed batch. A public post
/// fanning out re-read the same reachability row, signature preference and
/// signer keys once per job; a batch reads each once. Everything cached is
/// either immutable over the seconds a batch takes (key material, the
/// 30-day-scoped signature preference) or **written through** on every
/// change: the reachability row doubles as the circuit breaker, so
/// `record_success`/`record_failure` update cache and database together and a
/// host that fails job N is seen failed by job N+1, exactly as the uncached
/// worker behaved. Deliberately per-job forever: `job::still_queued` (a
/// cancellation must always be re-checked), `can_federate_url` (the domain
/// policy gate), and `try_claim_probe` (an atomic DB claim).
#[derive(Default)]
struct BatchCache {
    /// Per-host reachability rows; `None` = the host has no row (healthy).
    reachability: Mutex<HashMap<String, Option<HostReachability>>>,
    /// Per-host RFC 9421 knock verdicts, downgraded write-through.
    rfc9421: Mutex<HashMap<String, bool>>,
    /// Per-account signer rows; `None` = the signing account vanished.
    user_signers: Mutex<HashMap<i64, Option<Arc<Account>>>>,
    /// Decrypted key material is short-lived, zeroized, and reused only for
    /// this claimed batch. Without this cache the normalized repository adds
    /// two database reads (RSA + Ed25519) to every fan-out job.
    signing_keys: tokio::sync::Mutex<HashMap<(i64, bool), Arc<CachedSigningKeys>>>,
    /// The instance actor signs account-less deliveries with the same
    /// batch-bounded reuse rule.
    instance_signing_keys: tokio::sync::Mutex<Option<(bool, Arc<CachedSigningKeys>)>>,
}

struct CachedSigningKeys {
    private_key_pem: Zeroizing<String>,
    key_id: String,
    ed25519: Option<(Zeroizing<String>, String)>,
}

impl BatchCache {
    /// The host's reachability row, read once per batch. Errors are not
    /// cached — the caller keeps its fail-open handling per job.
    async fn reachability(
        &self,
        pool: &PgPool,
        host: &str,
    ) -> Result<Option<HostReachability>, DbError> {
        if let Some(row) = self.reachability.lock_or_recover().get(host) {
            return Ok(row.clone());
        }
        let row = reachability::find(pool, host).await?;
        self.reachability
            .lock()
            .unwrap()
            .insert(host.to_owned(), row.clone());
        Ok(row)
    }

    /// Write-through for [`reachability::record_failure`]'s returned row.
    fn store_reachability(&self, host: &str, row: HostReachability) {
        self.reachability
            .lock()
            .unwrap()
            .insert(host.to_owned(), Some(row));
    }

    /// Write-through for [`reachability::record_success`], which resets any
    /// existing row to the clean state (and never inserts one).
    fn note_success(&self, host: &str) {
        if let Some(Some(row)) = self.reachability.lock_or_recover().get_mut(host) {
            row.consecutive_failures = 0;
            row.first_failure_at = None;
            row.unreachable_since = None;
            row.next_probe_at = None;
            row.abandoned_at = None;
            row.last_success_at = Some(time::OffsetDateTime::now_utc());
        }
    }

    async fn should_try_rfc9421(&self, pool: &PgPool, host: &str) -> Result<bool, DbError> {
        if let Some(verdict) = self.rfc9421.lock_or_recover().get(host) {
            return Ok(*verdict);
        }
        let verdict = plamenu_db::signature_prefs::should_try_rfc9421(pool, host).await?;
        self.rfc9421
            .lock()
            .unwrap()
            .insert(host.to_owned(), verdict);
        Ok(verdict)
    }

    /// Write-through for the hard-refusal downgrade recorded after a knock
    /// falls back — later jobs in the batch stop paying the rejected knock.
    fn note_rfc9421_downgrade(&self, host: &str) {
        self.rfc9421
            .lock_or_recover()
            .insert(host.to_owned(), false);
    }

    async fn user_signer(
        &self,
        pool: &PgPool,
        account_id: i64,
    ) -> Result<Option<Arc<Account>>, DbError> {
        if let Some(signer) = self.user_signers.lock_or_recover().get(&account_id) {
            return Ok(signer.clone());
        }
        let signer = account::find_by_id(pool, account_id).await?.map(Arc::new);
        self.user_signers
            .lock()
            .unwrap()
            .insert(account_id, signer.clone());
        Ok(signer)
    }

    async fn account_signing_keys(
        &self,
        state: &AppState,
        account_id: i64,
        emit_integrity_proofs: bool,
    ) -> Result<Arc<CachedSigningKeys>, DeliveryFailure> {
        let cache_key = (account_id, emit_integrity_proofs);
        let mut cache = self.signing_keys.lock().await;
        if let Some(material) = cache.get(&cache_key) {
            return Ok(material.clone());
        }
        let rsa = crate::key_store::account_signing_key(state, account_id, "rsa")
            .await
            .map_err(DeliveryFailure::local)?;
        let private_key_pem = Zeroizing::new(
            rsa.private
                .expose_str()
                .map_err(DeliveryFailure::local)?
                .to_owned(),
        );
        let ed25519 = if emit_integrity_proofs {
            let key = crate::key_store::account_signing_key(state, account_id, "ed25519")
                .await
                .map_err(DeliveryFailure::local)?;
            Some((
                Zeroizing::new(
                    key.private
                        .expose_str()
                        .map_err(DeliveryFailure::local)?
                        .to_owned(),
                ),
                key.record.key_uri,
            ))
        } else {
            None
        };
        let material = Arc::new(CachedSigningKeys {
            private_key_pem,
            key_id: rsa.record.key_uri,
            ed25519,
        });
        cache.insert(cache_key, material.clone());
        Ok(material)
    }

    async fn instance_signing_keys(
        &self,
        state: &AppState,
        emit_integrity_proofs: bool,
    ) -> Result<Arc<CachedSigningKeys>, DeliveryFailure> {
        let mut cache = self.instance_signing_keys.lock().await;
        if let Some((cached_proof_setting, material)) = cache.as_ref()
            && *cached_proof_setting == emit_integrity_proofs
        {
            return Ok(material.clone());
        }
        let keyring = state.federation_keyring.as_deref().ok_or_else(|| {
            DeliveryFailure::local(crate::crypto::KeyEncryptionError::MissingConfiguration)
        })?;
        crate::key_store::ensure_instance(&state.pool, keyring, &state.config.domain)
            .await
            .map_err(DeliveryFailure::local)?;
        let rsa = crate::key_store::instance_signing_key(state, "rsa")
            .await
            .map_err(DeliveryFailure::local)?;
        let private_key_pem = Zeroizing::new(
            rsa.private
                .expose_str()
                .map_err(DeliveryFailure::local)?
                .to_owned(),
        );
        let ed25519 = if emit_integrity_proofs {
            let key = crate::key_store::instance_signing_key(state, "ed25519")
                .await
                .map_err(DeliveryFailure::local)?;
            Some((
                Zeroizing::new(
                    key.private
                        .expose_str()
                        .map_err(DeliveryFailure::local)?
                        .to_owned(),
                ),
                key.record.key_uri,
            ))
        } else {
            None
        };
        let material = Arc::new(CachedSigningKeys {
            private_key_pem,
            key_id: rsa.record.key_uri,
            ed25519,
        });
        *cache = Some((emit_integrity_proofs, material.clone()));
        Ok(material)
    }
}

/// Claims and attempts one batch of due deliveries; returns how many jobs
/// were claimed (0 = the queue is currently drained).
pub async fn run_due(state: &AppState) -> u64 {
    let jobs = match job::claim_due(&state.pool, BATCH_SIZE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "failed to claim delivery jobs");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    run_groups(state, group_by_inbox(jobs)).await;
    claimed
}

async fn run_groups(state: &AppState, groups: Vec<Vec<job::DeliveryJob>>) {
    // One cache per claimed batch: staleness is bounded by the batch, and the
    // write-through rules above keep the breaker exact within it.
    let cache = Arc::new(BatchCache::default());
    let mut tasks = JoinSet::new();
    for group in groups {
        while tasks.len() >= CONCURRENT_INBOXES {
            join_next_group(&mut tasks).await;
        }
        let state = state.clone();
        let cache = cache.clone();
        tasks.spawn(async move {
            run_group(&state, &cache, group).await;
        });
    }
    while !tasks.is_empty() {
        join_next_group(&mut tasks).await;
    }
}

async fn join_next_group(tasks: &mut JoinSet<()>) {
    if let Some(result) = tasks.join_next().await
        && let Err(error) = result
    {
        tracing::error!(%error, "delivery inbox task failed");
    }
}

async fn run_group(state: &AppState, cache: &BatchCache, jobs: Vec<job::DeliveryJob>) {
    for delivery_job in jobs {
        process_job(state, cache, &delivery_job).await;
    }
}

fn group_by_inbox(jobs: Vec<job::DeliveryJob>) -> Vec<Vec<job::DeliveryJob>> {
    let mut groups: Vec<Vec<job::DeliveryJob>> = Vec::new();
    for delivery_job in jobs {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].inbox_url == delivery_job.inbox_url)
        {
            group.push(delivery_job);
        } else {
            groups.push(vec![delivery_job]);
        }
    }
    groups
}

async fn process_job(state: &AppState, cache: &BatchCache, delivery_job: &job::DeliveryJob) {
    match attempt(state, cache, delivery_job).await {
        Ok(outcome) => {
            if matches!(outcome, AttemptOutcome::Delivered)
                && let Some(host) = inbox_host(&delivery_job.inbox_url)
            {
                match reachability::record_success(&state.pool, &host).await {
                    Ok(()) => cache.note_success(&host),
                    Err(error) => {
                        tracing::warn!(%error, host, "failed to record delivery success");
                    }
                }
            }
            if let Err(error) = job::complete(&state.pool, delivery_job.id).await {
                // The lease will make it due again; delivering twice is fine
                // (inbox processing is idempotent).
                tracing::warn!(%error, "delivered but failed to remove job");
            }
        }
        Err(failure) => {
            record_failure(state, cache, delivery_job, &failure).await;
            dispose_failed_job(state, delivery_job, &failure).await;
        }
    }
}

/// Feeds a remote-attributable failure into the host reachability record and
/// logs the breaker opening.
async fn record_failure(
    state: &AppState,
    cache: &BatchCache,
    delivery_job: &job::DeliveryJob,
    failure: &DeliveryFailure,
) {
    if !failure.remote {
        return;
    }
    let Some(host) = inbox_host(&delivery_job.inbox_url) else {
        return;
    };
    match reachability::record_failure(
        &state.pool,
        &host,
        failure.reachability_class(),
        &failure.message,
    )
    .await
    {
        Ok(row) => {
            cache.store_reachability(&host, row.clone());
            // Both stamps come from `now()` in the same statement exactly
            // when this failure opened the breaker.
            if row.unreachable_since.is_some() && row.unreachable_since == row.last_failure_at {
                tracing::warn!(
                    host,
                    failures = row.consecutive_failures,
                    "host marked unreachable; low-value deliveries will be \
                     skipped after the grace window"
                );
            }
        }
        Err(error) => tracing::warn!(%error, host, "failed to record delivery failure"),
    }
}

async fn dispose_failed_job(
    state: &AppState,
    delivery_job: &job::DeliveryJob,
    failure: &DeliveryFailure,
) {
    let error = &failure.message;
    match failure.class {
        FailureClass::Gone | FailureClass::Permanent => {
            tracing::info!(
                error,
                inbox = %delivery_job.inbox_url,
                class = failure.reachability_class(),
                "dropping undeliverable activity (terminal response)"
            );
            let _ = job::complete(&state.pool, delivery_job.id).await;
        }
        FailureClass::RateLimited { retry_after_secs }
            if delivery_job.attempts < job::MAX_ATTEMPTS =>
        {
            tracing::debug!(
                error,
                inbox = %delivery_job.inbox_url,
                retry_after_secs,
                "delivery rate-limited; honoring Retry-After"
            );
            #[allow(
                clippy::cast_precision_loss,
                reason = "Retry-After beyond f64 precision is capped anyway"
            )]
            let min_delay = retry_after_secs.unwrap_or(0) as f64;
            let _ = job::retry_no_sooner_than(
                &state.pool,
                delivery_job.id,
                delivery_job.attempts,
                min_delay,
            )
            .await;
        }
        _ if delivery_job.attempts >= job::MAX_ATTEMPTS => {
            tracing::warn!(
                error,
                inbox = %delivery_job.inbox_url,
                attempts = delivery_job.attempts,
                "dropping undeliverable activity"
            );
            let _ = job::complete(&state.pool, delivery_job.id).await;
        }
        _ => {
            tracing::debug!(
                error,
                inbox = %delivery_job.inbox_url,
                attempts = delivery_job.attempts,
                "delivery failed; will retry"
            );
            let _ = job::retry_later(&state.pool, delivery_job.id, delivery_job.attempts).await;
        }
    }
}

/// The circuit-breaker gate: while a host has been unreachable past the
/// grace window, low-value fan-out is dropped, with one probe per interval
/// let through so recovery is noticed. High-value deliveries always pass.
/// Any gate error fails open — never lose a delivery to breaker bookkeeping.
async fn breaker_says_skip(
    state: &AppState,
    cache: &BatchCache,
    delivery_job: &job::DeliveryJob,
) -> bool {
    let Some(host) = inbox_host(&delivery_job.inbox_url) else {
        return false;
    };
    let row = match cache.reachability(&state.pool, &host).await {
        Ok(Some(row)) => row,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(%error, host, "reachability lookup failed; delivering anyway");
            return false;
        }
    };
    let Some(since) = row.unreachable_since else {
        return false;
    };
    if row.abandoned_at.is_some() {
        tracing::info!(
            host,
            inbox = %delivery_job.inbox_url,
            "skipping delivery to permanently abandoned host"
        );
        return true;
    }
    if time::OffsetDateTime::now_utc() - since < SKIP_UNREACHABLE_AFTER
        || is_high_value(&delivery_job.activity)
    {
        return false;
    }
    match reachability::try_claim_probe(&state.pool, &host, PROBE_INTERVAL_SECS).await {
        Ok(true) => {
            tracing::info!(host, "probing unreachable host with a queued delivery");
            false
        }
        Ok(false) => {
            tracing::info!(
                host,
                inbox = %delivery_job.inbox_url,
                "skipping low-value delivery to unreachable host"
            );
            true
        }
        Err(error) => {
            tracing::warn!(%error, host, "probe claim failed; delivering anyway");
            false
        }
    }
}

/// The signer's key material for one delivery: the RSA pair for the HTTP
/// signature, the optional Ed25519 pair (with its keyId) for the FEP-8b32
/// proof, and extra request headers.
struct SigningMaterial {
    private_key_pem: Zeroizing<String>,
    key_id: String,
    ed25519: Option<(Zeroizing<String>, String)>,
    headers: Vec<(String, String)>,
    /// Local activities are built through compatibility APIs that still take
    /// a username. Rebase them to this persisted actor identity immediately
    /// before proof signing and transport signing.
    local_identity: Option<(String, Option<String>)>,
}

/// A job with no owning account is signed by the instance actor (Mastodon's
/// report anonymity for `Flag`); every other delivery is signed by the
/// local user whose action produced it.
async fn signing_material(
    state: &AppState,
    cache: &BatchCache,
    delivery_job: &job::DeliveryJob,
) -> Result<SigningMaterial, DeliveryFailure> {
    if let Some(account_id) = delivery_job.account_id {
        let signer = cache
            .user_signer(&state.pool, account_id)
            .await
            .map_err(DeliveryFailure::local)?
            .ok_or_else(|| DeliveryFailure::local("signing account vanished"))?;
        // A FEP-ae97 account is remotely owned even though it participates as
        // an ordinary account here. The delivery queue uses its gateway RSA
        // key, while its JSON is
        // already proof-signed by Minimitra's did:key and every identifier is
        // portable; rebasing or adding a Plamenu proof would violate the
        // gateway's byte-preserving delivery requirement.
        // Registration is the only path that persists an account with this
        // reserved actor URI. Deriving the kind from the already batch-cached
        // signer avoids turning the gateway check into one query per fan-out
        // job.
        let gateway_prefix = format!(
            "https://{}/.well-known/apgateway/did:key:",
            state.config.domain
        );
        let portable_gateway = signer
            .uri
            .as_deref()
            .is_some_and(|uri| uri.starts_with(&gateway_prefix));
        let emit_integrity_proofs = !portable_gateway && state.emit_integrity_proofs().await;
        let keys = cache
            .account_signing_keys(state, account_id, emit_integrity_proofs)
            .await?;
        let headers = if !portable_gateway
            && delivery_job.synchronize_followers
            && let Some(header) = crate::followers_sync::synchronization_header(
                state,
                &signer,
                &delivery_job.inbox_url,
            )
            .await
            .map_err(DeliveryFailure::local)?
        {
            vec![(crate::followers_sync::HEADER_NAME.to_owned(), header)]
        } else {
            Vec::new()
        };
        Ok(SigningMaterial {
            private_key_pem: keys.private_key_pem.clone(),
            key_id: keys.key_id.clone(),
            ed25519: keys.ed25519.clone(),
            headers,
            local_identity: (!portable_gateway)
                .then(|| (signer.username.clone(), signer.uri.clone())),
        })
    } else {
        let emit_integrity_proofs = state.emit_integrity_proofs().await;
        let keys = cache
            .instance_signing_keys(state, emit_integrity_proofs)
            .await?;
        Ok(SigningMaterial {
            private_key_pem: keys.private_key_pem.clone(),
            key_id: keys.key_id.clone(),
            ed25519: keys.ed25519.clone(),
            headers: Vec::new(),
            local_identity: None,
        })
    }
}

/// FEP-8b32: binds the payload to its actor's Ed25519 key, so receivers
/// (and anyone the receiver forwards to) can authenticate the copy without
/// re-fetching the origin. An activity that already carries a proof travels
/// as-is; a signing failure falls back to proof-less delivery rather than
/// blocking the queue.
fn with_integrity_proof(activity: &Value, ed25519: Option<(Zeroizing<String>, String)>) -> Value {
    match ed25519 {
        Some((ed25519_private, ed25519_key_id)) if activity.get("proof").is_none() => {
            match plamenu_ap::proof::sign_document(
                activity,
                &ed25519_private,
                &ed25519_key_id,
                &proof_timestamp(),
            ) {
                Ok(signed) => signed,
                Err(error) => {
                    tracing::warn!(%error, "integrity-proof signing failed; delivering without");
                    activity.clone()
                }
            }
        }
        _ => activity.clone(),
    }
}

async fn attempt(
    state: &AppState,
    cache: &BatchCache,
    delivery_job: &job::DeliveryJob,
) -> Result<AttemptOutcome, DeliveryFailure> {
    if !crate::instance_policy::can_federate_url(
        &state.pool,
        &state.config.domain,
        &delivery_job.inbox_url,
    )
    .await
    .map_err(DeliveryFailure::local)?
    {
        tracing::info!(
            inbox = %delivery_job.inbox_url,
            "skipping delivery to blocked or non-allowed domain"
        );
        return Ok(AttemptOutcome::Skipped);
    }
    if breaker_says_skip(state, cache, delivery_job).await {
        return Ok(AttemptOutcome::Skipped);
    }
    let material = signing_material(state, cache, delivery_job).await?;
    let canonical_activity = match &material.local_identity {
        Some((username, actor_id)) => plamenu_ap::activity::rebase_local_identity(
            &delivery_job.activity,
            &state.config.domain,
            username,
            actor_id.as_deref(),
        ),
        None => delivery_job.activity.clone(),
    };
    let activity = with_integrity_proof(&canonical_activity, material.ed25519);
    // A retraction may have cancelled this job after the claim (job::cancel
    // deletes the row); a cancelled activity must not go out.
    if !job::still_queued(&state.pool, delivery_job.id)
        .await
        .map_err(DeliveryFailure::local)?
    {
        tracing::debug!(
            inbox = %delivery_job.inbox_url,
            "delivery cancelled after claim; skipping"
        );
        return Ok(AttemptOutcome::Skipped);
    }
    // RFC 9421 double-knocking: knock unless emission is off or the host
    // refused it recently. Preference bookkeeping failing open/closed must
    // never lose a delivery.
    let host = inbox_host(&delivery_job.inbox_url);
    let try_rfc9421 = if state.emit_rfc9421().await {
        match &host {
            Some(host) => cache
                .should_try_rfc9421(&state.pool, host)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, host, "signature-pref lookup failed; knocking anyway");
                    true
                }),
            None => false,
        }
    } else {
        false
    };
    let hidden_service = crate::hidden_gate::is_hidden_url(&delivery_job.inbox_url);
    let style = state
        .federation
        .deliver(Delivery {
            activity,
            inbox_url: delivery_job.inbox_url.clone(),
            headers: material.headers,
            private_key_pem: material.private_key_pem,
            key_id: material.key_id,
            try_rfc9421,
        })
        .await
        .map_err(|e| DeliveryFailure::from_federation(&e, hidden_service))?;
    // The outbound path never records a *positive* verdict: a `200` is not
    // proof (a peer can 200 an inbox POST then asynchronously drop an activity
    // whose signature it couldn't parse — upstream Pleroma does this on RFC
    // 9421). RFC 9421 support is learned only from inbound observation (see
    // `signature_prefs`). The one safe outbound write is a *downgrade*: a
    // known-positive host we knocked with 9421 that hard-refused (non-2xx) and
    // fell back to cavage has lost the capability — record `false` so we stop
    // paying the rejected knock. Only ever on a real fallback, never on a 200.
    if try_rfc9421
        && style == plamenu_federation::SignatureStyle::Cavage
        && let Some(host) = &host
    {
        match plamenu_db::signature_prefs::record(&state.pool, host, false).await {
            Ok(()) => cache.note_rfc9421_downgrade(host),
            Err(error) => {
                tracing::warn!(%error, host, "failed to record signature-preference downgrade");
            }
        }
    }
    Ok(AttemptOutcome::Delivered)
}

/// `created` for an FEP-8b32 proof: RFC 3339 with whole seconds, the form
/// receivers that round-trip the stamp through their own datetime types
/// (Mitra's chrono structs) reproduce byte-for-byte during verification.
fn proof_timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("zero nanoseconds is valid")
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamps format")
}

/// Runs the delivery loop until the process exits.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("delivery worker started");
        loop {
            match listen_and_run(&state).await {
                // Ok only comes back on shutdown: stop without reconnecting.
                Ok(()) => return,
                Err(error) => {
                    if state.shutdown.is_cancelled() {
                        return;
                    }
                    tracing::warn!(%error, "delivery listener disconnected; reconnecting");
                    if !crate::workers::pause(&state, LISTENER_RECONNECT_DELAY).await {
                        return;
                    }
                }
            }
        }
    })
}

async fn listen_and_run(state: &AppState) -> Result<(), plamenu_db::DbError> {
    let mut listener = job::listener(&state.pool).await?;
    loop {
        // Stop claiming new batches the moment shutdown is signalled; the
        // batch already in flight finishes first.
        while !state.shutdown.is_cancelled() && run_due(state).await > 0 {}
        if state.shutdown.is_cancelled() {
            return Ok(());
        }
        wait_for_work(state, &mut listener).await?;
    }
}

async fn wait_for_work(
    state: &AppState,
    listener: &mut PgListener,
) -> Result<(), plamenu_db::DbError> {
    match job::next_due_delay(&state.pool).await? {
        Some(delay) => {
            tokio::select! {
                message = listener.recv() => {
                    message?;
                }
                () = tokio::time::sleep(delay) => {}
                () = state.shutdown.cancelled() => {}
            }
        }
        None => {
            tokio::select! {
                message = listener.recv() => {
                    message?;
                }
                () = state.shutdown.cancelled() => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use plamenu_ap::acct::Acct;
    use plamenu_ap::actor::RemoteActor;
    use plamenu_db::PgPool;
    use plamenu_federation::{FederationError, FetchedMedia, FetchedPage, ResolvedAcct};
    use serde_json::json;

    use super::*;
    use crate::Config;
    use crate::federation::{BoxFuture, FederationApi, WebPush};
    use crate::storage::MemoryStore;

    fn delivery_job(id: i64, inbox_url: &str) -> job::DeliveryJob {
        job::DeliveryJob {
            id,
            account_id: Some(1),
            inbox_url: inbox_url.to_owned(),
            activity: json!({ "id": id }),
            synchronize_followers: false,
            attempts: 1,
        }
    }

    #[test]
    fn grouping_preserves_inbox_and_claim_order() {
        let groups = group_by_inbox(vec![
            delivery_job(1, "https://a.example/inbox"),
            delivery_job(2, "https://b.example/inbox"),
            delivery_job(3, "https://a.example/inbox"),
            delivery_job(4, "https://b.example/inbox"),
            delivery_job(5, "https://c.example/inbox"),
        ]);

        let grouped_ids: Vec<Vec<i64>> = groups
            .iter()
            .map(|group| group.iter().map(|job| job.id).collect())
            .collect();
        assert_eq!(grouped_ids, vec![vec![1, 3], vec![2, 4], vec![5]]);
    }

    async fn local_account(pool: &PgPool) -> i64 {
        let rsa = plamenu_ap::keys::generate_keypair().unwrap();
        let ed = plamenu_ap::keys::generate_ed25519_keypair();
        let config = test_config();
        let keyring = crate::crypto::FederationKeyring::from_config(&config).unwrap();
        let mut tx = pool.begin().await.unwrap();
        let account = account::create_local_normalized_legacy(
            &mut *tx,
            account::NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: &rsa.public_pem,
            },
        )
        .await
        .unwrap();
        crate::key_store::provision_account_tx(
            &mut tx,
            &keyring,
            "plamenu.test",
            &account,
            &rsa,
            &ed,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        account.id
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn wait_for_work_returns_on_notify(pool: PgPool) {
        let state = state_with(&pool, &Arc::new(CountingFederation::default()));
        let mut listener = job::listener(&pool).await.unwrap();
        let account_id = local_account(&pool).await;

        let waiter = wait_for_work(&state, &mut listener);
        tokio::pin!(waiter);
        tokio::select! {
            result = &mut waiter => panic!("idle waiter returned before notify: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        job::enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"type": "Accept"}),
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn wait_for_work_returns_for_due_job_without_notify(pool: PgPool) {
        let account_id = local_account(&pool).await;
        job::enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"type": "Accept"}),
        )
        .await
        .unwrap();
        let mut listener = job::listener(&pool).await.unwrap();

        let state = state_with(&pool, &Arc::new(CountingFederation::default()));
        tokio::time::timeout(Duration::from_secs(1), wait_for_work(&state, &mut listener))
            .await
            .unwrap()
            .unwrap();
    }

    /// How the stub's `deliver` should fail (rebuilt into a fresh
    /// `FederationError` per call — the error type is not `Clone`).
    #[derive(Clone, Copy)]
    enum FailSpec {
        Status(u16),
        RateLimited(Option<u64>),
    }

    #[derive(Default)]
    struct CountingFederation {
        current: AtomicUsize,
        max_seen: AtomicUsize,
        deliveries: Mutex<Vec<Delivery>>,
        by_inbox: Mutex<HashMap<String, usize>>,
        max_by_inbox: Mutex<HashMap<String, usize>>,
        /// When set, every delivery fails this way.
        fail_with: Mutex<Option<FailSpec>>,
        /// The signature style successful deliveries report (the transport's
        /// double-knock verdict, simulated).
        deliver_style: Mutex<Option<plamenu_federation::SignatureStyle>>,
        /// When set, the next delivery cancels this queued (inbox, activity
        /// uri) mid-flight — simulates a retraction landing after the claim.
        cancel_during_delivery: Mutex<Option<(PgPool, String, String)>>,
    }

    impl CountingFederation {
        fn enter(&self, inbox: &str) {
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(current, Ordering::SeqCst);

            let mut by_inbox = self.by_inbox.lock_or_recover();
            let inbox_current = by_inbox.entry(inbox.to_owned()).or_default();
            *inbox_current += 1;
            let inbox_current = *inbox_current;
            drop(by_inbox);

            let mut max_by_inbox = self.max_by_inbox.lock_or_recover();
            let max = max_by_inbox.entry(inbox.to_owned()).or_default();
            *max = (*max).max(inbox_current);
        }

        fn leave(&self, inbox: &str) {
            let mut by_inbox = self.by_inbox.lock_or_recover();
            *by_inbox.get_mut(inbox).unwrap() -= 1;
            self.current.fetch_sub(1, Ordering::SeqCst);
        }

        fn max_seen(&self) -> usize {
            self.max_seen.load(Ordering::SeqCst)
        }

        fn max_for_inbox(&self, inbox: &str) -> usize {
            self.max_by_inbox
                .lock()
                .unwrap()
                .get(inbox)
                .copied()
                .unwrap_or_default()
        }
    }

    impl FederationApi for CountingFederation {
        fn fetch_actor<'a>(
            &'a self,
            _uri: &'a str,
        ) -> BoxFuture<'a, Result<RemoteActor, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn fetch_object<'a>(
            &'a self,
            _uri: &'a str,
        ) -> BoxFuture<'a, Result<serde_json::Value, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn fetch_object_following<'a>(
            &'a self,
            _uri: &'a str,
        ) -> BoxFuture<'a, Result<serde_json::Value, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn fetch_page<'a>(
            &'a self,
            _url: &'a str,
            _accept: &'a str,
        ) -> BoxFuture<'a, Result<FetchedPage, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn fetch_media<'a>(
            &'a self,
            _url: &'a str,
        ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn resolve_acct<'a>(
            &'a self,
            _acct: &'a Acct,
        ) -> BoxFuture<'a, Result<ResolvedAcct, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn deliver(
            &self,
            delivery: Delivery,
        ) -> BoxFuture<'_, Result<plamenu_federation::SignatureStyle, FederationError>> {
            self.deliveries.lock_or_recover().push(delivery.clone());
            Box::pin(async move {
                self.enter(&delivery.inbox_url);
                let cancel = self.cancel_during_delivery.lock_or_recover().take();
                if let Some((pool, inbox, uri)) = cancel {
                    job::cancel(&pool, &inbox, &uri).await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.leave(&delivery.inbox_url);
                match *self.fail_with.lock_or_recover() {
                    Some(FailSpec::Status(code)) => Err(FederationError::Status(code)),
                    Some(FailSpec::RateLimited(retry_after_secs)) => {
                        Err(FederationError::RateLimited { retry_after_secs })
                    }
                    None => Ok(self
                        .deliver_style
                        .lock()
                        .unwrap()
                        .unwrap_or(plamenu_federation::SignatureStyle::Cavage)),
                }
            })
        }

        fn web_push(&self, _push: WebPush) -> BoxFuture<'_, Result<u16, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn webhook(
            &self,
            _post: crate::federation::WebhookPost,
        ) -> BoxFuture<'_, Result<u16, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }

        fn service_request(
            &self,
            _request: crate::federation::ServiceRequest,
        ) -> BoxFuture<'_, Result<plamenu_federation::ServiceResponse, FederationError>> {
            Box::pin(async { Err(FederationError::Status(404)) })
        }
    }

    fn test_config() -> Config {
        Config {
            domain: "plamenu.test".to_owned(),
            account_domain: "plamenu.test".to_owned(),
            bind: "127.0.0.1:0".parse().unwrap(),
            database_url: String::new(),
            db_pool_size: 16,
            allow_private_fetch: true,
            authorized_fetch: false,
            authorized_fetch_unsigned_profile: false,
            media_dir: std::env::temp_dir(),
            ffmpeg_path: "ffmpeg".to_owned(),
            ffprobe_path: "ffprobe".to_owned(),
            smtp: None,
            trusted_proxies: vec!["127.0.0.0/8".to_owned(), "::1/128".to_owned()],
            encryption_secret: Some("delivery-test-encryption-secret-at-least-32-bytes".into()),
            encryption_secret_version: 1,
            encryption_previous_secrets: Vec::new(),
            update_check_url: None,
            translation: None,
            conversation_containers: false,
            csp_reporting: false,
            federation: crate::config::FederationConfig::default(),
        }
    }

    /// Pins the settings-owned emission flags (default on) for deterministic
    /// signature assertions; must run before the state's first settings read.
    async fn set_emissions(pool: &PgPool, proofs: bool, rfc9421: bool) {
        let current = plamenu_db::instance_settings::get(pool).await.unwrap();
        plamenu_db::instance_settings::save(
            pool,
            plamenu_db::instance_settings::SettingsUpdate {
                emit_integrity_proofs: proofs,
                emit_rfc9421: rfc9421,
                ..current.as_update()
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn run_due_overlaps_distinct_inboxes_but_serializes_each_inbox(pool: PgPool) {
        let account_id = local_account(&pool).await;
        for (inbox, id) in [
            ("https://a.example/inbox", 1),
            ("https://b.example/inbox", 2),
            ("https://a.example/inbox", 3),
        ] {
            job::enqueue(&pool, account_id, inbox, &json!({"id": id}))
                .await
                .unwrap();
        }

        let federation = Arc::new(CountingFederation::default());
        let state = AppState::new(
            pool.clone(),
            test_config(),
            federation.clone(),
            Arc::new(MemoryStore::default()),
        )
        .unwrap();

        assert_eq!(run_due(&state).await, 3);
        assert!(
            federation.max_seen() > 1,
            "different inbox groups should run concurrently"
        );
        assert_eq!(
            federation.max_for_inbox("https://a.example/inbox"),
            1,
            "same-inbox deliveries should stay serialized"
        );
        assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn job_cancelled_after_claim_is_not_delivered(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let inbox = "https://a.example/inbox";
        let announce_uri = "https://plamenu.test/users/alice/statuses/1/activity";
        job::enqueue(&pool, account_id, inbox, &json!({"id": "first"}))
            .await
            .unwrap();
        job::enqueue(&pool, account_id, inbox, &json!({"id": announce_uri}))
            .await
            .unwrap();

        let federation = Arc::new(CountingFederation::default());
        *federation.cancel_during_delivery.lock_or_recover() =
            Some((pool.clone(), inbox.to_owned(), announce_uri.to_owned()));
        let state = AppState::new(
            pool.clone(),
            test_config(),
            federation.clone(),
            Arc::new(MemoryStore::default()),
        )
        .unwrap();

        // Both jobs are claimed, but the second is cancelled while the first
        // is in flight — it must be skipped, not sent.
        assert_eq!(run_due(&state).await, 2);
        let delivered: Vec<String> = federation
            .deliveries
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.activity["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(delivered, vec!["first"]);
        assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
    }

    #[test]
    fn failure_classification_matches_the_response_taxonomy() {
        let class = |e: &FederationError| DeliveryFailure::from_federation(e, false).class;
        assert_eq!(class(&FederationError::Status(410)), FailureClass::Gone);
        assert_eq!(class(&FederationError::Status(404)), FailureClass::Gone);
        assert_eq!(
            class(&FederationError::Status(403)),
            FailureClass::Permanent
        );
        assert_eq!(
            class(&FederationError::Status(422)),
            FailureClass::Permanent
        );
        // 401 stays retryable: receiver-side key-fetch hiccups produce it.
        assert_eq!(
            class(&FederationError::Status(401)),
            FailureClass::Transient
        );
        assert_eq!(
            class(&FederationError::Status(500)),
            FailureClass::Transient
        );
        assert_eq!(
            class(&FederationError::RateLimited {
                retry_after_secs: Some(300)
            }),
            FailureClass::RateLimited {
                retry_after_secs: Some(300)
            }
        );
        // Local errors never blame the host.
        assert!(!DeliveryFailure::local("signing account vanished").remote);
        assert!(DeliveryFailure::from_federation(&FederationError::Status(500), false).remote);
        assert!(
            !DeliveryFailure::from_federation(&FederationError::InvalidUrl("x".into()), false)
                .remote
        );
    }

    /// Failure attribution for hidden-service inboxes (plan §7.1): a
    /// transport error reached the host through OUR Tor/I2P circuit, so it
    /// must not feed the host's reachability record — ordinary circuit churn
    /// would otherwise mark every onion peer unreachable. An HTTP answer
    /// (status, 429) means the service itself spoke and stays attributable.
    #[tokio::test]
    async fn hidden_service_transport_failures_are_not_remote_attributable() {
        // A real connect failure; `reqwest::Error` has no public constructor.
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await
            .expect_err("nothing listens on port 1");
        let transport = FederationError::Http(error);
        assert!(!DeliveryFailure::from_federation(&transport, true).remote);
        // The same failure on clearnet still counts against the host.
        assert!(DeliveryFailure::from_federation(&transport, false).remote);

        assert!(DeliveryFailure::from_federation(&FederationError::Status(500), true).remote);
        assert!(
            DeliveryFailure::from_federation(
                &FederationError::RateLimited {
                    retry_after_secs: None
                },
                true
            )
            .remote
        );
    }

    #[test]
    fn high_value_detection_keeps_mentions_and_relationship_changes() {
        for kind in [
            "Follow", "Accept", "Reject", "Undo", "Block", "Move", "Delete", "Flag",
        ] {
            assert!(is_high_value(&json!({"type": kind})), "{kind}");
        }
        // Public fan-out is low-value…
        assert!(!is_high_value(&json!({
            "type": "Create",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://plamenu.test/users/alice/followers"],
        })));
        // …but a mention/DM addressed to a specific actor is not.
        assert!(is_high_value(&json!({
            "type": "Create",
            "to": ["https://remote.example/users/bob"],
        })));
        assert!(is_high_value(&json!({
            "type": "Create",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://plamenu.test/users/alice/followers",
                   "https://remote.example/users/bob"],
        })));
        // Boosts and reactions stay low-value even though they cc the author.
        assert!(!is_high_value(&json!({
            "type": "Announce",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://remote.example/users/bob"],
        })));
        assert!(!is_high_value(&json!({"type": "Like"})));
    }

    fn state_with(pool: &PgPool, federation: &Arc<CountingFederation>) -> AppState {
        AppState::new(
            pool.clone(),
            test_config(),
            federation.clone(),
            Arc::new(MemoryStore::default()),
        )
        .unwrap()
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn terminal_responses_drop_the_job_on_first_attempt(pool: PgPool) {
        let account_id = local_account(&pool).await;
        for (code, class) in [(410, "gone"), (403, "permanent")] {
            let federation = Arc::new(CountingFederation::default());
            *federation.fail_with.lock_or_recover() = Some(FailSpec::Status(code));
            let state = state_with(&pool, &federation);
            job::enqueue(
                &pool,
                account_id,
                "https://a.example/inbox",
                &json!({"id": 1}),
            )
            .await
            .unwrap();

            assert_eq!(run_due(&state).await, 1);
            // One attempt, no retry left behind.
            assert_eq!(federation.deliveries.lock_or_recover().len(), 1, "{code}");
            assert_eq!(job::pending_count(&pool).await.unwrap(), 0, "{code}");
            let row = plamenu_db::reachability::find(&pool, "a.example")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.last_failure_class.as_deref(), Some(class), "{code}");
            // The host answered — the breaker never counts it.
            assert_eq!(row.consecutive_failures, 0, "{code}");
        }
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn rate_limited_delivery_honors_retry_after(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        *federation.fail_with.lock_or_recover() = Some(FailSpec::RateLimited(Some(300)));
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://a.example/inbox",
            &json!({"id": 1}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 1);
        // Still queued, and not due before the remote's Retry-After (300s),
        // which beats the first-attempt backoff (60s).
        assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
        let delay = job::next_due_delay(&pool).await.unwrap().unwrap();
        assert!(delay > Duration::from_secs(250), "{delay:?}");
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn transient_failures_keep_the_retry_schedule(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        *federation.fail_with.lock_or_recover() = Some(FailSpec::Status(500));
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://a.example/inbox",
            &json!({"id": 1}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 1);
        assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
        // A transient failure counts toward the breaker's streak.
        let row = plamenu_db::reachability::find(&pool, "a.example")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.consecutive_failures, 1);
        assert_eq!(row.last_failure_class.as_deref(), Some("transient"));
        assert!(row.unreachable_since.is_none());
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn open_breaker_skips_fanout_but_keeps_high_value_and_probes(pool: PgPool) {
        let account_id = local_account(&pool).await;
        // A host four days into an open breaker, with the probe slot taken.
        sqlx::query!(
            "INSERT INTO host_reachability
                 (host, consecutive_failures, unreachable_since, next_probe_at)
             VALUES ('down.example', 20, now() - interval '4 days',
                     now() + interval '1 hour')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let fanout = json!({
            "id": 1, "type": "Create",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": ["https://plamenu.test/users/alice/followers"],
        });
        let follow = json!({"id": 2, "type": "Follow"});

        // Low-value fan-out is dropped without touching the wire.
        let federation = Arc::new(CountingFederation::default());
        let state = state_with(&pool, &federation);
        job::enqueue(&pool, account_id, "https://down.example/inbox", &fanout)
            .await
            .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert_eq!(federation.deliveries.lock_or_recover().len(), 0);
        assert_eq!(job::pending_count(&pool).await.unwrap(), 0);

        // A high-value delivery still goes out (and, succeeding, closes the
        // breaker).
        job::enqueue(&pool, account_id, "https://down.example/inbox", &follow)
            .await
            .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert_eq!(federation.deliveries.lock_or_recover().len(), 1);
        let row = plamenu_db::reachability::find(&pool, "down.example")
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.unreachable_since.is_none(),
            "success closed the breaker"
        );
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn open_breaker_lets_one_probe_through_per_interval(pool: PgPool) {
        let account_id = local_account(&pool).await;
        // Probe slot free (`next_probe_at` NULL): the first low-value job is
        // the probe, the second is skipped.
        sqlx::query!(
            "INSERT INTO host_reachability (host, consecutive_failures, unreachable_since)
             VALUES ('down.example', 20, now() - interval '4 days')"
        )
        .execute(&pool)
        .await
        .unwrap();
        let fanout = |id: i64| {
            json!({
                "id": id, "type": "Create",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            })
        };

        let federation = Arc::new(CountingFederation::default());
        // The probe fails (the host is still down) so the breaker stays open.
        *federation.fail_with.lock_or_recover() = Some(FailSpec::Status(500));
        let state = state_with(&pool, &federation);
        job::enqueue(&pool, account_id, "https://down.example/inbox", &fanout(1))
            .await
            .unwrap();
        job::enqueue(&pool, account_id, "https://down.example/inbox", &fanout(2))
            .await
            .unwrap();

        assert_eq!(run_due(&state).await, 2);
        // Exactly one probe hit the wire; the probe failure re-queues it, the
        // skipped job is gone.
        assert_eq!(federation.deliveries.lock_or_recover().len(), 1);
        assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
        let row = plamenu_db::reachability::find(&pool, "down.example")
            .await
            .unwrap()
            .unwrap();
        assert!(row.unreachable_since.is_some());
        assert!(row.next_probe_at.is_some(), "probe slot is paced");
    }

    /// Write-through, success direction: the per-batch reachability cache
    /// must see a mid-batch success immediately. A high-value delivery that
    /// closes the breaker makes the *next* job in the same batch deliverable —
    /// exactly what the uncached worker did by re-reading the row per job. A
    /// stale cache would skip the second job here.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn success_mid_batch_closes_the_breaker_for_the_rest_of_the_batch(pool: PgPool) {
        let account_id = local_account(&pool).await;
        // Four days unreachable, probe slot taken: low-value only passes if
        // the breaker actually closes mid-batch.
        sqlx::query!(
            "INSERT INTO host_reachability
                 (host, consecutive_failures, unreachable_since, next_probe_at)
             VALUES ('down.example', 20, now() - interval '4 days',
                     now() + interval '1 hour')"
        )
        .execute(&pool)
        .await
        .unwrap();

        let federation = Arc::new(CountingFederation::default());
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://down.example/inbox",
            &json!({"id": 1, "type": "Follow"}),
        )
        .await
        .unwrap();
        job::enqueue(
            &pool,
            account_id,
            "https://down.example/inbox",
            &json!({
                "id": 2, "type": "Create",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            }),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 2);
        assert_eq!(
            federation.deliveries.lock_or_recover().len(),
            2,
            "the follow's success must unblock the same batch's fan-out"
        );
        assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
        let row = plamenu_db::reachability::find(&pool, "down.example")
            .await
            .unwrap()
            .unwrap();
        assert!(row.unreachable_since.is_none());
    }

    /// Write-through, failure direction: a mid-batch failure that tips the
    /// host into abandonment must stop the very next job of the same batch —
    /// even a high-value one, since an abandoned host skips everything.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn failure_mid_batch_marks_abandonment_for_the_rest_of_the_batch(pool: PgPool) {
        let account_id = local_account(&pool).await;
        // One transient failure away from abandonment, probe slot free.
        sqlx::query!(
            "INSERT INTO host_reachability (host, consecutive_failures, unreachable_since)
             VALUES ('flaky.example', $1, now() - interval '4 days')",
            reachability::ABANDON_AFTER_FAILURES - 1,
        )
        .execute(&pool)
        .await
        .unwrap();

        let federation = Arc::new(CountingFederation::default());
        *federation.fail_with.lock_or_recover() = Some(FailSpec::Status(500));
        let state = state_with(&pool, &federation);
        // The low-value probe fails and abandons the host; the follow behind
        // it in the same batch must then be skipped, not delivered.
        job::enqueue(
            &pool,
            account_id,
            "https://flaky.example/inbox",
            &json!({
                "id": 1, "type": "Create",
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
            }),
        )
        .await
        .unwrap();
        job::enqueue(
            &pool,
            account_id,
            "https://flaky.example/inbox",
            &json!({"id": 2, "type": "Follow"}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 2);
        assert_eq!(
            federation.deliveries.lock_or_recover().len(),
            1,
            "only the probe may reach the wire; abandonment must stop the follow"
        );
        let row = plamenu_db::reachability::find(&pool, "flaky.example")
            .await
            .unwrap()
            .unwrap();
        assert!(row.abandoned_at.is_some(), "the probe failure abandons");
        // The failed probe is requeued for retry; the skipped follow is gone.
        assert_eq!(job::pending_count(&pool).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn abandoned_host_skips_every_delivery_without_probing(pool: PgPool) {
        let account_id = local_account(&pool).await;
        sqlx::query!(
            "INSERT INTO host_reachability
                 (host, consecutive_failures, unreachable_since, abandoned_at)
             VALUES ('gone.example', $1, now() - interval '4 days', now())",
            reachability::ABANDON_AFTER_FAILURES,
        )
        .execute(&pool)
        .await
        .unwrap();
        let federation = Arc::new(CountingFederation::default());
        let state = state_with(&pool, &federation);
        for activity in [
            json!({"id": 1, "type": "Create", "to": ["https://www.w3.org/ns/activitystreams#Public"]}),
            json!({"id": 2, "type": "Follow"}),
        ] {
            job::enqueue(&pool, account_id, "https://gone.example/inbox", &activity)
                .await
                .unwrap();
        }
        assert_eq!(run_due(&state).await, 2);
        assert!(federation.deliveries.lock_or_recover().is_empty());
        assert_eq!(job::pending_count(&pool).await.unwrap(), 0);
    }

    /// With `emit_integrity_proofs` on and the signer holding an
    /// Ed25519 pair, the delivered activity carries a verifiable
    /// `eddsa-jcs-2022` proof bound to the signer's `#ed25519-key`.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn integrity_proof_is_attached_when_enabled(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        set_emissions(&pool, true, false).await;
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"id": "https://plamenu.test/a/1", "type": "Follow",
                    "actor": "https://plamenu.test/users/alice"}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 1);
        let activity = federation.deliveries.lock_or_recover()[0].activity.clone();
        assert_eq!(activity["proof"]["cryptosuite"], json!("eddsa-jcs-2022"));
        assert_eq!(
            activity["proof"]["verificationMethod"],
            json!("https://plamenu.test/users/alice#ed25519-key")
        );
        let keys = plamenu_db::actor_key::usable_for_account(&pool, account_id)
            .await
            .unwrap()
            .into_iter()
            .find(|key| key.algorithm == "ed25519")
            .unwrap();
        plamenu_ap::proof::PreparedProof::from_document(&activity)
            .unwrap()
            .verify(&keys.public_key)
            .unwrap();
    }

    /// Missing proof-signing material fails closed and leaves the job for
    /// retry; it is never downgraded to an unauthenticated proof-less copy.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn missing_ed25519_key_delivers_without_proof(pool: PgPool) {
        let account_id = local_account(&pool).await;
        sqlx::query("DELETE FROM actor_keys WHERE account_id = $1 AND algorithm = 'ed25519'")
            .bind(account_id)
            .execute(&pool)
            .await
            .unwrap();
        let federation = Arc::new(CountingFederation::default());
        set_emissions(&pool, true, false).await;
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"id": 1, "type": "Follow"}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 1);
        let deliveries = federation.deliveries.lock_or_recover();
        assert!(deliveries.is_empty());
    }

    /// With both emission flags switched off in the settings, activities
    /// travel unchanged and deliveries never knock.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn no_proof_when_disabled(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        set_emissions(&pool, false, false).await;
        let state = state_with(&pool, &federation);
        job::enqueue(
            &pool,
            account_id,
            "https://remote.example/inbox",
            &json!({"id": 1, "type": "Follow"}),
        )
        .await
        .unwrap();

        assert_eq!(run_due(&state).await, 1);
        let deliveries = federation.deliveries.lock_or_recover();
        assert!(deliveries[0].activity.get("proof").is_none());
        assert!(
            !deliveries[0].try_rfc9421,
            "9421 knocking is disabled in settings"
        );
    }

    /// Approach (a): with `emit_rfc9421` on, an *unknown* host still gets
    /// draft-cavage (no knock) — a successful delivery is not proof of RFC
    /// 9421 support, so nothing is recorded. Only a host with a positive
    /// row (earned via inbound observation) is knocked.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn unknown_host_gets_cavage_and_records_nothing(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        set_emissions(&pool, false, true).await;
        let state = state_with(&pool, &federation);

        *federation.deliver_style.lock_or_recover() =
            Some(plamenu_federation::SignatureStyle::Cavage);
        job::enqueue(
            &pool,
            account_id,
            "https://pleroma.example/inbox",
            &json!({"id": 1, "type": "Follow"}),
        )
        .await
        .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert!(
            !federation.deliveries.lock_or_recover()[0].try_rfc9421,
            "an unknown host is not knocked (cavage default)"
        );
        // The Pleroma trap: even a `200` must never mint a positive row.
        assert!(
            plamenu_db::signature_prefs::find(&pool, "pleroma.example")
                .await
                .unwrap()
                .is_none(),
            "a successful cavage delivery records no preference"
        );
    }

    /// A host proven to speak RFC 9421 (positive row, as inbound observation
    /// would leave) is knocked. If that knock later hard-refuses and falls
    /// back to cavage, the row is downgraded so we stop paying the knock.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn positive_host_is_knocked_then_downgraded_on_hard_refusal(pool: PgPool) {
        let account_id = local_account(&pool).await;
        let federation = Arc::new(CountingFederation::default());
        set_emissions(&pool, false, true).await;
        let state = state_with(&pool, &federation);

        // Inbound observation earlier left a positive row for this host.
        plamenu_db::signature_prefs::record(&pool, "modern.example", true)
            .await
            .unwrap();

        // First delivery: the host is knocked and the transport confirms 9421.
        *federation.deliver_style.lock_or_recover() =
            Some(plamenu_federation::SignatureStyle::Rfc9421);
        job::enqueue(
            &pool,
            account_id,
            "https://modern.example/inbox",
            &json!({"id": 1, "type": "Follow"}),
        )
        .await
        .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert!(federation.deliveries.lock_or_recover()[0].try_rfc9421);
        assert!(
            plamenu_db::signature_prefs::should_try_rfc9421(&pool, "modern.example")
                .await
                .unwrap()
        );

        // The host later hard-refuses 9421 and the transport falls back to
        // cavage: record the downgrade so the next delivery skips the knock.
        *federation.deliver_style.lock_or_recover() =
            Some(plamenu_federation::SignatureStyle::Cavage);
        job::enqueue(
            &pool,
            account_id,
            "https://modern.example/inbox",
            &json!({"id": 2, "type": "Follow"}),
        )
        .await
        .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert!(
            federation.deliveries.lock_or_recover()[1].try_rfc9421,
            "still knocked before the downgrade lands"
        );
        assert!(
            !plamenu_db::signature_prefs::should_try_rfc9421(&pool, "modern.example")
                .await
                .unwrap(),
            "a hard-refusal fallback downgrades the host to cavage"
        );

        job::enqueue(
            &pool,
            account_id,
            "https://modern.example/inbox",
            &json!({"id": 3, "type": "Follow"}),
        )
        .await
        .unwrap();
        assert_eq!(run_due(&state).await, 1);
        assert!(
            !federation.deliveries.lock_or_recover()[2].try_rfc9421,
            "the downgrade suppresses the knock"
        );
    }
}
