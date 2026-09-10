//! Bounded, demand-driven hydration of a remote actor's outbox.
//!
//! Request handlers only enqueue. This low-priority worker performs at most an
//! outbox envelope plus one collection page per job, under the federation
//! client's shared admission/SSRF/failure controls.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt};
use plamenu_ap::activity::{attributed_to_id, id_of, visibility_from_addressing};
use plamenu_db::account::{self, Account};
use plamenu_db::remote_history::{self, Admission, ClaimedJob, EnqueueOutcome, JobKind, Success};
use plamenu_federation::{FederationError, FetchedActivityPub};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinHandle;
use url::Url;

use crate::AppState;
use crate::error::ApiError;
use crate::ingest::{
    RemoteIngestContext, ingest_remote_note_in_context, resolve_or_fetch_status,
    update_remote_note_in_context,
};
use crate::remote::host_of;

const PAGE_ITEMS: usize = 20;
const BARE_IRIS: usize = 5;
const CLAIM_BATCH: usize = 4;
const JOB_WALL_TIME: Duration = Duration::from_secs(30);
const IDLE_POLL: Duration = Duration::from_secs(2);
const PRUNE_INTERVAL: Duration = Duration::from_hours(1);
const PRUNE_BATCH: i64 = 200;
const INTENT_MIN_INTERVAL: Duration = Duration::from_mins(5);
const MAX_INFLIGHT_INTENTS: usize = 256;
const INTENT_STAMP_CAP: usize = 4096;

/// In-process admission for the best-effort hydration intent inferred from a
/// compatible API timeline read. The durable job queue remains authoritative;
/// this small guard only prevents rapid client refreshes from creating an
/// unbounded number of detached database tasks before that queue can coalesce
/// them.
#[derive(Default)]
pub struct IntentCoordinator {
    inner: Arc<Mutex<IntentState>>,
}

#[derive(Default)]
struct IntentState {
    in_flight: HashSet<i64>,
    last_request: HashMap<i64, Instant>,
}

impl IntentCoordinator {
    fn try_admit(&self, account_id: i64) -> Option<IntentGuard> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.last_request.len() >= INTENT_STAMP_CAP {
            state
                .last_request
                .retain(|_, at| at.elapsed() < INTENT_MIN_INTERVAL);
        }
        if state
            .last_request
            .get(&account_id)
            .is_some_and(|at| at.elapsed() < INTENT_MIN_INTERVAL)
            || state.in_flight.contains(&account_id)
            || state.in_flight.len() >= MAX_INFLIGHT_INTENTS
        {
            return None;
        }
        state.in_flight.insert(account_id);
        state.last_request.insert(account_id, Instant::now());
        Some(IntentGuard {
            inner: Arc::clone(&self.inner),
            account_id,
        })
    }
}

struct IntentGuard {
    inner: Arc<Mutex<IntentState>>,
    account_id: i64,
}

impl Drop for IntentGuard {
    fn drop(&mut self) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .in_flight
            .remove(&self.account_id);
    }
}

#[derive(Default)]
struct Metrics {
    requested: AtomicU64,
    enqueued: AtomicU64,
    coalesced: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    pages: AtomicU64,
    bytes: AtomicU64,
    inline_items: AtomicU64,
    iri_dereferences: AtomicU64,
    accepted: AtomicU64,
    promoted: AtomicU64,
    pruned: AtomicU64,
    elapsed_ms: AtomicU64,
    intent_to_first_ms: AtomicU64,
    first_history_jobs: AtomicU64,
    reasons: Mutex<HashMap<String, u64>>,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::default);

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub requested: u64,
    pub enqueued: u64,
    pub coalesced: u64,
    pub completed: u64,
    pub failed: u64,
    pub pages: u64,
    pub bytes: u64,
    pub inline_items: u64,
    pub iri_dereferences: u64,
    pub accepted: u64,
    pub promoted: u64,
    pub pruned: u64,
    pub average_job_ms: u64,
    pub average_intent_to_first_ms: u64,
    pub reasons: HashMap<String, u64>,
}

#[must_use]
pub fn metrics() -> MetricsSnapshot {
    let completed = METRICS.completed.load(Ordering::Relaxed);
    MetricsSnapshot {
        requested: METRICS.requested.load(Ordering::Relaxed),
        enqueued: METRICS.enqueued.load(Ordering::Relaxed),
        coalesced: METRICS.coalesced.load(Ordering::Relaxed),
        completed,
        failed: METRICS.failed.load(Ordering::Relaxed),
        pages: METRICS.pages.load(Ordering::Relaxed),
        bytes: METRICS.bytes.load(Ordering::Relaxed),
        inline_items: METRICS.inline_items.load(Ordering::Relaxed),
        iri_dereferences: METRICS.iri_dereferences.load(Ordering::Relaxed),
        accepted: METRICS.accepted.load(Ordering::Relaxed),
        promoted: METRICS.promoted.load(Ordering::Relaxed),
        pruned: METRICS.pruned.load(Ordering::Relaxed),
        average_job_ms: METRICS
            .elapsed_ms
            .load(Ordering::Relaxed)
            .checked_div(completed)
            .unwrap_or(0),
        average_intent_to_first_ms: METRICS
            .intent_to_first_ms
            .load(Ordering::Relaxed)
            .checked_div(METRICS.first_history_jobs.load(Ordering::Relaxed))
            .unwrap_or(0),
        reasons: METRICS
            .reasons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    }
}

fn reason(name: &str) {
    let mut reasons = METRICS
        .reasons
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *reasons.entry(name.to_owned()).or_default() += 1;
}

pub(crate) fn record_promotion() {
    METRICS.promoted.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_pruned(count: u64) {
    METRICS.pruned.fetch_add(count, Ordering::Relaxed);
}

fn origin_of(uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    matches!(url.scheme(), "https" | "http").then(|| url.origin().ascii_serialization())
}

fn same_origin(uri: &str, origin: &str) -> bool {
    origin_of(uri).as_deref() == Some(origin)
}

fn eligible(account: &Account, local_domain: &str) -> bool {
    account.domain.is_some()
        && !account.is_portable_on(local_domain)
        && !account.suspended()
        && matches!(account.actor_type.as_deref(), Some("Person" | "Service"))
}

/// Enqueues initial/refresh/older work after local policy and actor checks.
/// It performs no federation request.
pub async fn request(
    state: &AppState,
    account: &Account,
    kind: JobKind,
    requested_by: Option<i64>,
) -> Result<EnqueueOutcome, ApiError> {
    request_with_admission(state, account, kind, requested_by, Admission::Explicit).await
}

/// Submits a passive profile-view hint. Unlike [`request`], this path obeys a
/// durable automatic cooldown so repeated web/API reads cannot churn a
/// terminal result back into queued work.
pub async fn request_automatic(
    state: &AppState,
    account: &Account,
    kind: JobKind,
    requested_by: Option<i64>,
) -> Result<EnqueueOutcome, ApiError> {
    request_with_admission(state, account, kind, requested_by, Admission::Automatic).await
}

async fn request_with_admission(
    state: &AppState,
    account: &Account,
    kind: JobKind,
    requested_by: Option<i64>,
    admission: Admission,
) -> Result<EnqueueOutcome, ApiError> {
    METRICS.requested.fetch_add(1, Ordering::Relaxed);
    if !eligible(account, &state.config.domain) {
        reason("unsupported_actor");
        return Err(ApiError::Conflict(
            "remote history supports Person and Service actors".into(),
        ));
    }
    let snapshot = remote_history::snapshot(&state.pool, account.id)
        .await?
        .ok_or_else(|| ApiError::Conflict("remote actor has no outbox".into()))?;
    let uri = if kind == JobKind::Older {
        snapshot.next_page_uri.as_deref()
    } else {
        snapshot.outbox_uri.as_deref()
    }
    .ok_or_else(|| ApiError::Conflict("remote actor has no fetchable history page".into()))?;
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri).await? {
        reason("instance_policy");
        return Err(ApiError::Conflict(
            "remote origin is blocked by instance policy".into(),
        ));
    }
    let origin = origin_of(uri)
        .ok_or_else(|| ApiError::Conflict("remote outbox has an invalid origin".into()))?;
    let page_uri = (kind == JobKind::Older).then_some(uri);
    let outcome = remote_history::enqueue(
        &state.pool,
        account.id,
        kind,
        page_uri,
        &origin,
        requested_by,
        admission,
    )
    .await?;
    match outcome {
        EnqueueOutcome::Enqueued => {
            METRICS.enqueued.fetch_add(1, Ordering::Relaxed);
        }
        EnqueueOutcome::Coalesced => {
            METRICS.coalesced.fetch_add(1, Ordering::Relaxed);
        }
        EnqueueOutcome::Fresh => reason("fresh"),
        EnqueueOutcome::Disabled => reason("disabled"),
        EnqueueOutcome::Backoff(_) => reason("origin_backoff"),
        EnqueueOutcome::AutomaticCooldown(_) => reason("automatic_cooldown"),
        EnqueueOutcome::OriginBusy(_) => reason("origin_queue_full"),
        EnqueueOutcome::RateLimited(_) => reason("user_rate_limit"),
    }
    Ok(outcome)
}

/// Records a third-party client's remote-profile view and submits its initial
/// hydration intent without extending the foreground account-status request.
/// Admission is bounded and process-local; the resulting queue entry is
/// durable and owns the federation-side coalescing, limits, and backoff.
pub fn spawn_initial_request(state: &AppState, account: &Account, requested_by: i64) {
    let Some(guard) = state.remote_history_intent.try_admit(account.id) else {
        return;
    };
    let state = state.clone();
    let account = account.clone();
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(error) = remote_history::touch_viewed(&state.pool, account.id).await {
            tracing::debug!(
                account = account.id,
                error = %error,
                "remote profile history view touch skipped for API client"
            );
        }
        if let Err(error) =
            request_automatic(&state, &account, JobKind::Initial, Some(requested_by)).await
        {
            tracing::debug!(
                account = account.id,
                error = %error.chain(),
                "remote profile history enqueue skipped for API client"
            );
        }
    });
}

#[derive(Debug)]
struct JobError {
    class: &'static str,
    unsupported: bool,
    retry_at: Option<OffsetDateTime>,
    detail: String,
}

impl JobError {
    fn invalid(class: &'static str, detail: impl Into<String>) -> Self {
        Self {
            class,
            unsupported: true,
            retry_at: None,
            detail: detail.into(),
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "map_err transfers the owned API error"
    )]
    fn ingest(error: ApiError) -> Self {
        Self {
            class: "ingest",
            unsupported: false,
            retry_at: None,
            detail: error.chain().to_string(),
        }
    }
}

impl From<FederationError> for JobError {
    fn from(error: FederationError) -> Self {
        let (class, unsupported, retry_at) = match &error {
            FederationError::RateLimited { retry_after_secs } => {
                let seconds = retry_after_secs.unwrap_or(60).clamp(1, 86_400);
                (
                    "rate_limited",
                    false,
                    Some(
                        OffsetDateTime::now_utc()
                            + time::Duration::seconds(i64::try_from(seconds).unwrap_or(86_400)),
                    ),
                )
            }
            FederationError::Status(401 | 403 | 404 | 410) => ("unavailable", true, None),
            FederationError::InvalidActor(_)
            | FederationError::InvalidUrl(_)
            | FederationError::PrivateAddress(_)
            | FederationError::DocumentTooLarge(_) => ("invalid_document", true, None),
            FederationError::Status(500..=599)
            | FederationError::Http(_)
            | FederationError::Stalled(_)
            | FederationError::FetchSuppressed(_)
            | FederationError::Admission(_) => ("transport", false, None),
            _ => ("fetch", false, None),
        };
        Self {
            class,
            unsupported,
            retry_at,
            detail: error.to_string(),
        }
    }
}

fn collection_items(page: &Value) -> Vec<Value> {
    match page.get("orderedItems").or_else(|| page.get("items")) {
        Some(Value::Array(items)) => items.clone(),
        Some(value) => vec![value.clone()],
        None => Vec::new(),
    }
}

fn supported_collection(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("OrderedCollection" | "Collection" | "OrderedCollectionPage" | "CollectionPage")
    )
}

fn collection_link(value: Option<&Value>) -> Option<&str> {
    value.and_then(id_of)
}

fn actor_matches(account: &Account, value: &Value) -> bool {
    let expected = account.uri.as_deref();
    value
        .get("actor")
        .and_then(id_of)
        .is_none_or(|actor| Some(actor) == expected)
}

fn valid_post_attribution(object: &Value, author: &Account) -> bool {
    let Some(uri) = id_of(object) else {
        return false;
    };
    let Some(attributed) = attributed_to_id(object) else {
        return false;
    };
    Some(attributed) == author.uri.as_deref() && host_of(uri) == host_of(attributed)
}

async fn fetch_history_document(
    state: &AppState,
    uri: &str,
    etag: Option<&str>,
) -> Result<FetchedActivityPub, JobError> {
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
    {
        return Err(JobError::invalid(
            "instance_policy",
            "remote history URL is blocked by current instance policy",
        ));
    }
    state
        .federation
        .fetch_activitypub(uri, etag)
        .await
        .map_err(JobError::from)
}

async fn store_post(state: &AppState, author: &Account, object: &Value) -> Result<bool, JobError> {
    if !crate::ingest::is_ingestible_note(object) || !valid_post_attribution(object, author) {
        reason("invalid_attribution");
        return Ok(false);
    }
    let followers = account::collection_urls_of(&state.pool, author.id)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
        .map(|urls| urls.followers_url)
        .unwrap_or_default();
    let visibility = visibility_from_addressing(
        object.get("to").unwrap_or(&Value::Null),
        object.get("cc").unwrap_or(&Value::Null),
        &followers,
    );
    if !matches!(visibility, "public" | "unlisted") {
        reason("non_public_item");
        return Ok(false);
    }
    let uri = id_of(object).expect("validated id");
    if !crate::instance_policy::can_federate_url(&state.pool, &state.config.domain, uri)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
    {
        reason("instance_policy");
        return Ok(false);
    }
    if let Some(existing) = plamenu_db::status::find_by_uri(&state.pool, uri)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
    {
        if existing.account_id != author.id {
            reason("canonical_author_mismatch");
            return Ok(false);
        }
        update_remote_note_in_context(
            state,
            author,
            &existing,
            object,
            RemoteIngestContext::History,
        )
        .await
        .map_err(JobError::ingest)?;
    } else {
        ingest_remote_note_in_context(state, author, object, RemoteIngestContext::History)
            .await
            .map_err(JobError::ingest)?;
    }
    Ok(true)
}

async fn dereference_bare(
    state: &AppState,
    uri: &str,
    origin: &str,
    bare_count: &mut usize,
    enabled: bool,
) -> Result<Option<Value>, JobError> {
    if !enabled || *bare_count >= BARE_IRIS || !same_origin(uri, origin) {
        reason(if enabled {
            "bare_iri_cap_or_origin"
        } else {
            "bare_iri_disabled"
        });
        return Ok(None);
    }
    *bare_count += 1;
    METRICS.iri_dereferences.fetch_add(1, Ordering::Relaxed);
    let fetched = fetch_history_document(state, uri, None).await?;
    METRICS
        .bytes
        .fetch_add(fetched.body_bytes as u64, Ordering::Relaxed);
    Ok(fetched.document)
}

/// Resolves the post named by a historical `Announce`. An embedded target can
/// be stored directly when its author is already known; otherwise the target's
/// canonical URI is fetched through the same path as an inbox `Announce`.
async fn resolve_announce_target(
    state: &AppState,
    object: &Value,
) -> Result<Option<plamenu_db::status::Status>, JobError> {
    let Some(uri) = id_of(object) else {
        reason("announce_target_without_id");
        return Ok(None);
    };
    if let Some(target) = plamenu_db::status::find_by_uri(&state.pool, uri)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
    {
        return Ok(Some(target));
    }
    if let Value::Object(_) = object
        && let Some(attributed) = attributed_to_id(object)
        && let Some(author) = account::find_by_uri(&state.pool, attributed)
            .await
            .map_err(ApiError::from)
            .map_err(JobError::ingest)?
        && store_post(state, &author, object).await?
    {
        return plamenu_db::status::find_by_uri(&state.pool, uri)
            .await
            .map_err(ApiError::from)
            .map_err(JobError::ingest);
    }
    resolve_or_fetch_status(state, uri)
        .await
        .map_err(JobError::ingest)
}

async fn process_item(
    state: &AppState,
    outbox_author: &Account,
    mut item: Value,
    collection_origin: &str,
    bare_count: &mut usize,
    bare_enabled: bool,
) -> Result<bool, JobError> {
    if let Some(uri) = item.as_str() {
        let Some(fetched) =
            dereference_bare(state, uri, collection_origin, bare_count, bare_enabled).await?
        else {
            return Ok(false);
        };
        item = fetched;
    } else {
        METRICS.inline_items.fetch_add(1, Ordering::Relaxed);
    }
    match item.get("type").and_then(Value::as_str) {
        Some("Create" | "Update") => {
            if !actor_matches(outbox_author, &item) {
                reason("activity_actor_mismatch");
                return Ok(false);
            }
            let mut object = item.get("object").cloned().unwrap_or(Value::Null);
            if let Some(uri) = object.as_str() {
                let Some(fetched) =
                    dereference_bare(state, uri, collection_origin, bare_count, bare_enabled)
                        .await?
                else {
                    return Ok(false);
                };
                object = fetched;
            }
            store_post(state, outbox_author, &object).await
        }
        Some("Announce") => {
            if !actor_matches(outbox_author, &item) {
                reason("activity_actor_mismatch");
                return Ok(false);
            }
            let Some(activity_uri) = id_of(&item) else {
                reason("announce_without_id");
                return Ok(false);
            };
            let target = match item.get("object") {
                Some(object @ (Value::String(_) | Value::Object(_))) => {
                    resolve_announce_target(state, object).await?
                }
                _ => None,
            };
            let Some(target) = target else {
                reason("announce_target_uncached");
                return Ok(false);
            };
            let published = item
                .get("published")
                .and_then(Value::as_str)
                .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok());
            plamenu_db::status::upsert_remote_reblog_history(
                &state.pool,
                activity_uri,
                outbox_author.id,
                target.id,
                published,
            )
            .await
            .map_err(ApiError::from)
            .map_err(JobError::ingest)?;
            Ok(true)
        }
        Some(_) => store_post(state, outbox_author, &item).await,
        None => {
            reason("missing_type");
            Ok(false)
        }
    }
}

async fn process_page(
    state: &AppState,
    author: &Account,
    page: &Value,
    page_uri: &str,
    origin: &str,
    bare_enabled: bool,
    reject_seen: bool,
) -> Result<(u64, u64, Option<String>), JobError> {
    if !supported_collection(page) {
        return Err(JobError::invalid(
            "invalid_collection_type",
            "outbox page is not an ActivityStreams collection or collection page",
        ));
    }
    if !same_origin(page_uri, origin) {
        return Err(JobError::invalid(
            "cross_origin_page",
            "collection page redirected outside the canonical outbox origin",
        ));
    }
    if let Some(part_of) = collection_link(page.get("partOf"))
        && !same_origin(part_of, origin)
    {
        return Err(JobError::invalid(
            "cross_origin_part_of",
            "collection partOf left the canonical outbox origin",
        ));
    }
    if reject_seen
        && remote_history::page_seen(&state.pool, author.id, page_uri)
            .await
            .map_err(ApiError::from)
            .map_err(JobError::ingest)?
    {
        reason("cursor_cycle");
        return Ok((0, 0, None));
    }
    let items = collection_items(page);
    let seen = items.len().min(PAGE_ITEMS) as u64;
    let mut accepted = 0u64;
    let mut bare_count = 0usize;
    for item in items.into_iter().take(PAGE_ITEMS) {
        if process_item(state, author, item, origin, &mut bare_count, bare_enabled).await? {
            accepted += 1;
        }
    }
    let mut next = collection_link(page.get("next")).map(str::to_owned);
    if let Some(next) = &next
        && !same_origin(next, origin)
    {
        return Err(JobError::invalid(
            "cross_origin_cursor",
            "collection next cursor left the canonical outbox origin",
        ));
    }
    if let Some(next_uri) = next.as_deref()
        && (next_uri == page_uri
            || remote_history::page_seen(&state.pool, author.id, next_uri)
                .await
                .map_err(ApiError::from)
                .map_err(JobError::ingest)?)
    {
        reason("cursor_cycle");
        next = None;
    }
    Ok((seen, accepted, next))
}

#[allow(
    clippy::too_many_lines,
    reason = "one bounded collection state machine"
)]
async fn run_job(state: &AppState, job: &ClaimedJob) -> Result<Success, JobError> {
    let author = account::find_by_id(&state.pool, job.account_id)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
        .ok_or_else(|| JobError::invalid("missing_actor", "remote actor was deleted"))?;
    if !eligible(&author, &state.config.domain) {
        return Err(JobError::invalid(
            "unsupported_actor",
            "actor is no longer an eligible Person or Service",
        ));
    }
    let snapshot = remote_history::snapshot(&state.pool, author.id)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?
        .ok_or_else(|| JobError::invalid("missing_state", "history state was deleted"))?;
    let settings = remote_history::settings(&state.pool)
        .await
        .map_err(ApiError::from)
        .map_err(JobError::ingest)?;

    let mut bytes = 0u64;
    let mut outbox_etag = None;
    let mut first_page_etag = None;
    let mut first_page_uri = None;
    let mut total = None;

    let (page, page_uri, canonical_origin) = if job.kind == "older" {
        let uri = job
            .page_uri
            .as_deref()
            .ok_or_else(|| JobError::invalid("missing_cursor", "older job has no page URI"))?;
        let fetched = fetch_history_document(state, uri, None).await?;
        bytes += fetched.body_bytes as u64;
        let origin = origin_of(&fetched.final_url)
            .ok_or_else(|| JobError::invalid("invalid_origin", "page has no valid origin"))?;
        if origin != job.origin {
            return Err(JobError::invalid(
                "cross_origin_redirect",
                "older page redirected outside its queued origin",
            ));
        }
        let document = fetched.document.ok_or_else(|| {
            JobError::invalid("unexpected_304", "uncached older page returned 304")
        })?;
        (document, fetched.final_url, origin)
    } else {
        let uri = snapshot
            .outbox_uri
            .as_deref()
            .ok_or_else(|| JobError::invalid("missing_outbox", "actor has no outbox"))?;
        let fetched = fetch_history_document(state, uri, snapshot.outbox_etag.as_deref()).await?;
        bytes += fetched.body_bytes as u64;
        outbox_etag = fetched.etag.clone();
        let origin = origin_of(&fetched.final_url)
            .ok_or_else(|| JobError::invalid("invalid_origin", "outbox has no valid origin"))?;
        if fetched.document.is_none() {
            return Ok(Success {
                page_uri: None,
                first_page_uri: snapshot.first_page_uri,
                next_page_uri: snapshot.next_page_uri,
                outbox_etag,
                first_page_etag: snapshot.first_page_etag,
                reported_total_items: snapshot.reported_total_items,
                pages: 0,
                items_seen: 0,
                items_accepted: 0,
                bytes,
            });
        }
        let envelope = fetched.document.expect("checked Some");
        if !supported_collection(&envelope) {
            return Err(JobError::invalid(
                "invalid_collection_type",
                "outbox is not an ActivityStreams collection",
            ));
        }
        total = envelope
            .get("totalItems")
            .and_then(Value::as_u64)
            .and_then(|value| i64::try_from(value).ok());
        let first = envelope.get("first").cloned();
        match first {
            Some(first @ Value::Object(_)) => {
                let page_uri = collection_link(Some(&first))
                    .unwrap_or(fetched.final_url.as_str())
                    .to_owned();
                first_page_uri = Some(page_uri.clone());
                (first, page_uri, origin)
            }
            Some(value) => {
                let first_uri = id_of(&value).ok_or_else(|| {
                    JobError::invalid("invalid_first", "outbox first is not a collection link")
                })?;
                if !same_origin(first_uri, &origin) {
                    return Err(JobError::invalid(
                        "cross_origin_first",
                        "outbox first page left its canonical origin",
                    ));
                }
                let fetched_page =
                    fetch_history_document(state, first_uri, snapshot.first_page_etag.as_deref())
                        .await?;
                bytes += fetched_page.body_bytes as u64;
                if !same_origin(&fetched_page.final_url, &origin) {
                    return Err(JobError::invalid(
                        "cross_origin_redirect",
                        "first page redirected outside the canonical outbox origin",
                    ));
                }
                first_page_uri = Some(fetched_page.final_url.clone());
                first_page_etag = fetched_page.etag;
                let Some(document) = fetched_page.document else {
                    return Ok(Success {
                        page_uri: None,
                        first_page_uri,
                        next_page_uri: snapshot.next_page_uri,
                        outbox_etag,
                        first_page_etag,
                        reported_total_items: total.or(snapshot.reported_total_items),
                        pages: 0,
                        items_seen: 0,
                        items_accepted: 0,
                        bytes,
                    });
                };
                (document, fetched_page.final_url, origin)
            }
            None if !collection_items(&envelope).is_empty() => {
                first_page_uri = Some(fetched.final_url.clone());
                (envelope, fetched.final_url, origin)
            }
            None => {
                return Err(JobError::invalid(
                    "empty_collection",
                    "outbox has neither first page nor items",
                ));
            }
        }
    };

    let (seen, accepted, next) = process_page(
        state,
        &author,
        &page,
        &page_uri,
        &canonical_origin,
        settings.bare_iri_enabled,
        job.kind == "older",
    )
    .await?;
    Ok(Success {
        page_uri: Some(page_uri),
        first_page_uri,
        next_page_uri: next,
        outbox_etag,
        first_page_etag,
        reported_total_items: total,
        pages: 1,
        items_seen: seen,
        items_accepted: accepted,
        bytes,
    })
}

async fn finish_one(state: &AppState, job: ClaimedJob) {
    let started = Instant::now();
    let result = tokio::time::timeout(JOB_WALL_TIME, run_job(state, &job)).await;
    match result {
        Ok(Ok(success)) => {
            METRICS.completed.fetch_add(1, Ordering::Relaxed);
            METRICS.pages.fetch_add(success.pages, Ordering::Relaxed);
            METRICS.bytes.fetch_add(success.bytes, Ordering::Relaxed);
            METRICS
                .accepted
                .fetch_add(success.items_accepted, Ordering::Relaxed);
            if success.items_accepted > 0 {
                let elapsed = OffsetDateTime::now_utc() - job.created_at;
                METRICS.intent_to_first_ms.fetch_add(
                    u64::try_from(elapsed.whole_milliseconds()).unwrap_or(0),
                    Ordering::Relaxed,
                );
                METRICS.first_history_jobs.fetch_add(1, Ordering::Relaxed);
            }
            if let Err(error) = remote_history::finish_success(&state.pool, &job, &success).await {
                tracing::error!(job = job.id, account = job.account_id, %error, "remote history completion failed");
            } else {
                tracing::info!(
                    job = job.id,
                    account = job.account_id,
                    origin = %job.origin,
                    pages = success.pages,
                    items = success.items_accepted,
                    bytes = success.bytes,
                    "remote history job completed"
                );
            }
        }
        Ok(Err(error)) => {
            METRICS.failed.fetch_add(1, Ordering::Relaxed);
            reason(error.class);
            tracing::warn!(
                job = job.id,
                account = job.account_id,
                origin = %job.origin,
                class = error.class,
                error = %error.detail,
                "remote history job failed"
            );
            if let Err(db_error) = remote_history::finish_failure(
                &state.pool,
                &job,
                error.class,
                error.retry_at,
                error.unsupported,
            )
            .await
            {
                tracing::error!(job = job.id, %db_error, "remote history failure state could not be saved");
            }
        }
        Err(_) => {
            METRICS.failed.fetch_add(1, Ordering::Relaxed);
            reason("wall_timeout");
            let _ = remote_history::finish_failure(&state.pool, &job, "wall_timeout", None, false)
                .await;
        }
    }
    METRICS.elapsed_ms.fetch_add(
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

pub async fn run_due(state: &AppState) -> u64 {
    let worker = format!("remote-history-{}", std::process::id());
    let jobs = match remote_history::claim_due(
        &state.pool,
        &worker,
        i64::try_from(CLAIM_BATCH).unwrap_or(4),
    )
    .await
    {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::error!(%error, "remote history claim failed");
            return 0;
        }
    };
    let claimed = jobs.len() as u64;
    stream::iter(jobs)
        .for_each_concurrent(CLAIM_BATCH, |job| finish_one(state, job))
        .await;
    claimed
}

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("remote history worker started");
        let mut last_prune = Instant::now();
        loop {
            let claimed = Box::pin(run_due(&state)).await;
            if last_prune.elapsed() >= PRUNE_INTERVAL {
                if let Ok(settings) = remote_history::settings(&state.pool).await {
                    match remote_history::prune(&state.pool, settings.retention_days, PRUNE_BATCH)
                        .await
                    {
                        Ok(pruned) => {
                            record_pruned(pruned);
                        }
                        Err(error) => tracing::warn!(%error, "remote history pruning failed"),
                    }
                }
                last_prune = Instant::now();
            }
            if claimed == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn origins_include_effective_port() {
        assert_eq!(
            origin_of("https://Example.test:8443/outbox").as_deref(),
            Some("https://example.test:8443")
        );
        assert!(same_origin(
            "https://example.test:8443/page",
            "https://example.test:8443"
        ));
        assert!(!same_origin(
            "https://example.test/page",
            "https://example.test:8443"
        ));
    }

    #[test]
    fn collection_items_accepts_both_shapes() {
        assert_eq!(
            collection_items(&json!({"orderedItems": [{"type": "Note"}]})).len(),
            1
        );
        assert_eq!(
            collection_items(&json!({"items": "https://x.test/1"})).len(),
            1
        );
        assert!(collection_items(&json!({})).is_empty());
    }
}
