//! Shared fixtures: an in-memory federation stub and account helpers.
#![allow(dead_code)] // each test binary uses a different subset

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use plamenu::config::{SmtpConfig, Starttls};
use plamenu::federation::{
    BoxFuture, Delivery, FederationApi, ServiceRequest, WebPush, WebhookPost,
};
use plamenu::{AppState, Config, build_router};
use plamenu_ap::acct::Acct;
use plamenu_ap::actor::{PublicKey, RemoteActor, RemoteEndpoints};
use plamenu_ap::keys::{self, KeyPairPem};
use plamenu_db::account::{self, Account, NewLocalAccount};
use plamenu_db::{PgPool, instance_settings};
use plamenu_federation::{
    FederationError, FetchedActivityPub, FetchedMedia, FetchedMediaFile, FetchedMediaRange,
    FetchedPage, RequestSigner, ResolvedAcct, ServiceResponse, WebfingerCandidate,
};

pub const TEST_DOMAIN: &str = "plamenu.test";

/// A tracing layer that counts every `sqlx::query` event — every statement the
/// pool executes, on any thread — so a test can assert an endpoint's query
/// count stays flat over its input size instead of growing with a serial
/// per-id loop. Install with `tracing::subscriber::set_default`.
pub struct QueryCounter(pub Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for QueryCounter {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target().starts_with("sqlx::query") {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records every fetch and delivery; serves actors, objects and webfinger
/// lookups from fixed maps. `fail_deliveries` simulates an unreachable
/// remote.
#[derive(Default)]
pub struct StubFederation {
    pub actors: Mutex<HashMap<String, RemoteActor>>,
    pub objects: Mutex<HashMap<String, serde_json::Value>>,
    /// Optional validators for `fetch_activitypub`; a matching request `ETag`
    /// returns a bodyless 304-shaped result.
    pub activitypub_etags: Mutex<HashMap<String, String>>,
    pub conditional_fetches: Mutex<Vec<(String, Option<String>)>>,
    /// `WebFinger` candidates keyed by `acct` string. A single acct can hold
    /// several (a Lemmy-style Person + Group under one handle).
    pub webfinger: Mutex<HashMap<String, Vec<WebfingerCandidate>>>,
    /// Optional HLS alternate advertised alongside an acct's AP self link.
    pub webfinger_hls: Mutex<HashMap<String, String>>,
    /// `OStatus` subscribe templates keyed by `acct` string, for
    /// remote-interaction tests.
    pub subscribe_templates: Mutex<HashMap<String, String>>,
    pub fetches: Mutex<Vec<String>>,
    pub deliveries: Mutex<Vec<Delivery>>,
    pub fail_deliveries: Mutex<bool>,
    pub pushes: Mutex<Vec<WebPush>>,
    /// HTTP status the fake push service answers with (`None` = 201).
    pub push_status: Mutex<Option<u16>>,
    pub webhook_posts: Mutex<Vec<WebhookPost>>,
    /// HTTP status the fake webhook receiver answers with (`None` = 200).
    pub webhook_status: Mutex<Option<u16>>,
    /// Web pages served to `fetch_page` (link previews), keyed by URL.
    pub pages: Mutex<HashMap<String, FetchedPage>>,
    /// Every `fetch_page` URL requested, in order.
    pub page_fetches: Mutex<Vec<String>>,
    /// Binary media served to `fetch_media` (remote-media caching), keyed by URL.
    pub media: Mutex<HashMap<String, FetchedMedia>>,
    /// Every `fetch_media` URL requested, in order.
    pub media_fetches: Mutex<Vec<String>>,
    /// Canned `(status, body)` replies for `service_request` (translation
    /// backend), keyed by request URL.
    pub service_responses: Mutex<HashMap<String, (u16, String)>>,
    /// Every `service_request` made, in order.
    pub service_requests: Mutex<Vec<ServiceRequest>>,
    /// Every `fetch_range_to_file` call, in order: `(url, start, len)` — lets
    /// HLS-proxy tests assert N viewers cause ONE origin fetch per segment.
    pub range_fetches: Mutex<Vec<(String, u64, u64)>>,
    /// Optional chunking/delay for cancellation and backpressure tests.
    pub range_chunking: Mutex<Option<(usize, u64)>>,
    pub range_chunks_sent: Arc<AtomicUsize>,
    /// When set, every `fetch_object` blocks on this semaphore until a test
    /// opens it — a "slow origin" gate for proving the remote-refresh fan-out
    /// stays bounded while outbound object fetches hang.
    pub object_fetch_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// When set, `fetch_object_following` answers with a budget suppression
    /// while the budget-ignoring probe still serves the object map — models a
    /// suppressed target for federation-debug tests.
    pub suppress_object_fetches: Mutex<bool>,
}

impl StubFederation {
    /// The permalink-following lookup shared by `fetch_object_following` and
    /// its budget-ignoring probe: serve the body at `uri`, then if its `id`
    /// differs (a permalink), re-dereference and serve the canonical one.
    fn follow_object(&self, uri: &str) -> Result<serde_json::Value, FederationError> {
        self.fetches.lock().unwrap().push(uri.to_owned());
        let objects = self.objects.lock().unwrap();
        match objects.get(uri).cloned() {
            None => Err(FederationError::Status(404)),
            Some(body) => match body.get("id").and_then(serde_json::Value::as_str) {
                Some(id) if id == uri => Ok(body),
                Some(id) => {
                    self.fetches.lock().unwrap().push(id.to_owned());
                    objects.get(id).cloned().ok_or(FederationError::Status(404))
                }
                None => Err(FederationError::Status(404)),
            },
        }
    }

    pub fn with_actors(actors: impl IntoIterator<Item = RemoteActor>) -> Arc<Self> {
        let stub = Self::default();
        stub.actors
            .lock()
            .unwrap()
            .extend(actors.into_iter().map(|a| (a.id.clone(), a)));
        Arc::new(stub)
    }

    /// Registers a user for both webfinger resolution and actor fetching.
    pub fn with_users(users: &[&RemoteUser]) -> Arc<Self> {
        let stub = Self::default();
        for user in users {
            stub.actors
                .lock()
                .unwrap()
                .insert(user.actor.id.clone(), user.actor.clone());
            stub.webfinger
                .lock()
                .unwrap()
                .entry(user.acct.clone())
                .or_default()
                .push(WebfingerCandidate {
                    actor_uri: user.actor.id.clone(),
                    advertised_type: user.actor.actor_type().map(str::to_owned),
                });
        }
        Arc::new(stub)
    }

    pub fn fetches(&self) -> Vec<String> {
        self.fetches.lock().unwrap().clone()
    }

    pub fn serve_activitypub_etag(&self, uri: &str, etag: &str) {
        self.activitypub_etags
            .lock()
            .unwrap()
            .insert(uri.to_owned(), etag.to_owned());
    }

    pub fn conditional_fetches(&self) -> Vec<(String, Option<String>)> {
        self.conditional_fetches.lock().unwrap().clone()
    }

    pub fn range_fetches(&self) -> Vec<(String, u64, u64)> {
        self.range_fetches.lock().unwrap().clone()
    }

    pub fn slow_range_chunks(&self, bytes: usize, delay_ms: u64) {
        *self.range_chunking.lock().unwrap() = Some((bytes, delay_ms));
        self.range_chunks_sent.store(0, Ordering::SeqCst);
    }

    pub fn range_chunks_sent(&self) -> usize {
        self.range_chunks_sent.load(Ordering::SeqCst)
    }

    pub fn deliveries(&self) -> Vec<Delivery> {
        self.deliveries.lock().unwrap().clone()
    }

    pub fn set_fail_deliveries(&self, fail: bool) {
        *self.fail_deliveries.lock().unwrap() = fail;
    }

    pub fn pushes(&self) -> Vec<WebPush> {
        self.pushes.lock().unwrap().clone()
    }

    /// Serves an HTML page at `url` (no redirect) for link-preview fetches.
    pub fn serve_page(&self, url: &str, body: &str) {
        self.serve_page_as(url, url, "text/html; charset=utf-8", body);
    }

    /// Serves a page with an explicit final URL (redirect target) and
    /// content type.
    pub fn serve_page_as(&self, url: &str, final_url: &str, content_type: &str, body: &str) {
        self.pages.lock().unwrap().insert(
            url.to_owned(),
            FetchedPage {
                final_url: final_url.to_owned(),
                content_type: content_type.to_owned(),
                body: body.to_owned(),
            },
        );
    }

    pub fn page_fetches(&self) -> Vec<String> {
        self.page_fetches.lock().unwrap().clone()
    }

    /// Serves a binary media body at `url` for `fetch_media` (remote-media
    /// caching). The download pipeline detects the real type from the bytes,
    /// so `content_type` is only a placeholder header.
    pub fn serve_media(&self, url: &str, content_type: &str, bytes: Vec<u8>) {
        self.media.lock().unwrap().insert(
            url.to_owned(),
            FetchedMedia {
                final_url: url.to_owned(),
                content_type: content_type.to_owned(),
                bytes,
            },
        );
    }

    pub fn media_fetches(&self) -> Vec<String> {
        self.media_fetches.lock().unwrap().clone()
    }

    /// Registers a canned reply for a translation-backend `service_request`,
    /// keyed by the exact request URL.
    pub fn serve_service(&self, url: &str, status: u16, body: &str) {
        self.service_responses
            .lock()
            .unwrap()
            .insert(url.to_owned(), (status, body.to_owned()));
    }

    pub fn service_requests(&self) -> Vec<ServiceRequest> {
        self.service_requests.lock().unwrap().clone()
    }

    pub fn set_push_status(&self, status: u16) {
        *self.push_status.lock().unwrap() = Some(status);
    }

    /// Installs a closed gate so every subsequent `fetch_object` blocks until
    /// [`open_object_fetches`](Self::open_object_fetches) releases it. Lets a
    /// slow-origin test hold outbound object fetches open while it floods signed
    /// `Update`s, then assert the refresh fan-out stayed bounded.
    pub fn gate_object_fetches(&self) {
        *self.object_fetch_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    }

    /// Releases every gated (and future) `fetch_object`.
    pub fn open_object_fetches(&self) {
        if let Some(gate) = self.object_fetch_gate.lock().unwrap().as_ref() {
            gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        }
    }

    pub fn webhook_posts(&self) -> Vec<WebhookPost> {
        self.webhook_posts.lock().unwrap().clone()
    }

    pub fn set_webhook_status(&self, status: u16) {
        *self.webhook_status.lock().unwrap() = Some(status);
    }
}

impl FederationApi for StubFederation {
    fn fetch_actor<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<RemoteActor, FederationError>> {
        self.fetches.lock().unwrap().push(uri.to_owned());
        let result = self
            .actors
            .lock()
            .unwrap()
            .get(uri)
            .cloned()
            .ok_or(FederationError::Status(404));
        Box::pin(async move { result })
    }

    fn fetch_object<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, FederationError>> {
        self.fetches.lock().unwrap().push(uri.to_owned());
        let gate = self.object_fetch_gate.lock().unwrap().clone();
        let result = self
            .objects
            .lock()
            .unwrap()
            .get(uri)
            .cloned()
            .ok_or(FederationError::Status(404));
        Box::pin(async move {
            if let Some(gate) = gate {
                // Blocks until the test opens the gate; the permit is only the
                // wait signal, so it is dropped as soon as it is granted.
                let _ = gate.acquire().await;
            }
            result
        })
    }

    fn fetch_activitypub<'a>(
        &'a self,
        uri: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchedActivityPub, FederationError>> {
        self.fetches.lock().unwrap().push(uri.to_owned());
        self.conditional_fetches
            .lock()
            .unwrap()
            .push((uri.to_owned(), etag.map(str::to_owned)));
        let response_etag = self.activitypub_etags.lock().unwrap().get(uri).cloned();
        let not_modified = etag.is_some() && etag == response_etag.as_deref();
        let document = if not_modified {
            None
        } else {
            self.objects.lock().unwrap().get(uri).cloned()
        };
        let result = if document.is_some() || not_modified {
            Ok(FetchedActivityPub {
                body_bytes: document
                    .as_ref()
                    .and_then(|value| serde_json::to_vec(value).ok())
                    .map_or(0, |body| body.len()),
                document,
                etag: response_etag,
                final_url: uri.to_owned(),
            })
        } else {
            Err(FederationError::Status(404))
        };
        Box::pin(async move { result })
    }

    fn fetch_object_following<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, FederationError>> {
        let result = if *self.suppress_object_fetches.lock().unwrap() {
            Err(FederationError::FetchSuppressed(uri.to_owned()))
        } else {
            self.follow_object(uri)
        };
        Box::pin(async move { result })
    }

    fn fetch_object_following_ignoring_budget<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, FederationError>> {
        let result = self.follow_object(uri);
        Box::pin(async move { result })
    }

    fn fetch_page<'a>(
        &'a self,
        url: &'a str,
        _accept: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FederationError>> {
        self.page_fetches.lock().unwrap().push(url.to_owned());
        let result = self
            .pages
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or(FederationError::Status(404));
        Box::pin(async move { result })
    }

    fn fetch_media<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>> {
        self.media_fetches.lock().unwrap().push(url.to_owned());
        let result = self
            .media
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or(FederationError::Status(404));
        Box::pin(async move { result })
    }

    fn fetch_media_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaFile, FederationError>> {
        self.media_fetches.lock().unwrap().push(url.to_owned());
        let result = self
            .media
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or(FederationError::Status(404));
        let dest = dest.to_path_buf();
        Box::pin(async move {
            let fetched = result?;
            if fetched.bytes.len() as u64 > max_bytes {
                return Err(FederationError::TooLarge(url.to_owned()));
            }
            tokio::fs::write(&dest, &fetched.bytes).await?;
            Ok(FetchedMediaFile {
                final_url: fetched.final_url,
                content_type: fetched.content_type,
                bytes_written: fetched.bytes.len() as u64,
            })
        })
    }

    fn fetch_range_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<u64, FederationError>> {
        self.range_fetches
            .lock()
            .unwrap()
            .push((url.to_owned(), start, len));
        let full = self.media.lock().unwrap().get(url).map(|m| m.bytes.clone());
        let dest = dest.to_path_buf();
        Box::pin(async move {
            let full = full.ok_or(FederationError::Status(404))?;
            let s = usize::try_from(start).unwrap_or(usize::MAX).min(full.len());
            let e = usize::try_from(start.saturating_add(len))
                .unwrap_or(usize::MAX)
                .min(full.len());
            tokio::fs::write(&dest, &full[s..e]).await?;
            Ok((e - s) as u64)
        })
    }

    fn fetch_media_range<'a>(
        &'a self,
        url: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaRange, FederationError>> {
        self.range_fetches
            .lock()
            .unwrap()
            .push((url.to_owned(), start, len));
        let fetched = self.media.lock().unwrap().get(url).cloned();
        let chunking = *self.range_chunking.lock().unwrap();
        let chunks_sent = self.range_chunks_sent.clone();
        Box::pin(async move {
            let fetched = fetched.ok_or(FederationError::Status(404))?;
            let total_len = fetched.bytes.len() as u64;
            let end = start
                .checked_add(len)
                .filter(|end| *end <= total_len)
                .ok_or_else(|| FederationError::Status(416))?;
            let start_usize = usize::try_from(start).map_err(|_| FederationError::Status(416))?;
            let end_usize = usize::try_from(end).map_err(|_| FederationError::Status(416))?;
            let bytes = fetched.bytes[start_usize..end_usize].to_vec();
            let stream = futures_util::stream::unfold(
                (bytes, 0_usize, chunking, chunks_sent),
                |(bytes, offset, chunking, chunks_sent)| async move {
                    if offset >= bytes.len() {
                        return None;
                    }
                    let (chunk_size, delay_ms) = chunking.unwrap_or((bytes.len(), 0));
                    if delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    let end = (offset + chunk_size).min(bytes.len());
                    chunks_sent.fetch_add(1, Ordering::SeqCst);
                    let chunk = bytes[offset..end].to_vec();
                    Some((Ok(chunk), (bytes, end, chunking, chunks_sent)))
                },
            );
            Ok(FetchedMediaRange {
                final_url: fetched.final_url,
                content_type: fetched.content_type,
                total_len,
                start,
                len,
                bytes: Box::pin(stream),
            })
        })
    }

    fn resolve_acct<'a>(
        &'a self,
        acct: &'a Acct,
    ) -> BoxFuture<'a, Result<ResolvedAcct, FederationError>> {
        let result = self
            .webfinger
            .lock()
            .unwrap()
            .get(&acct.to_string())
            .filter(|candidates| !candidates.is_empty())
            .map(|candidates| {
                let candidates = candidates.clone();
                // Same primary pick as the real client: first person-like, else
                // first candidate.
                let actor_uri = candidates
                    .iter()
                    .find(|c| !c.is_group_hint())
                    .unwrap_or(&candidates[0])
                    .actor_uri
                    .clone();
                ResolvedAcct {
                    acct: acct.clone(),
                    actor_uri,
                    candidates,
                    subscribe_template: self
                        .subscribe_templates
                        .lock()
                        .unwrap()
                        .get(&acct.to_string())
                        .cloned(),
                    hls_stream_url: self
                        .webfinger_hls
                        .lock()
                        .unwrap()
                        .get(&acct.to_string())
                        .cloned(),
                }
            })
            .ok_or(FederationError::Status(404));
        Box::pin(async move { result })
    }

    fn deliver(
        &self,
        delivery: Delivery,
    ) -> BoxFuture<'_, Result<plamenu_federation::SignatureStyle, FederationError>> {
        if *self.fail_deliveries.lock().unwrap() {
            return Box::pin(async { Err(FederationError::Status(503)) });
        }
        self.deliveries.lock().unwrap().push(delivery);
        Box::pin(async { Ok(plamenu_federation::SignatureStyle::Cavage) })
    }

    fn web_push(&self, push: WebPush) -> BoxFuture<'_, Result<u16, FederationError>> {
        self.pushes.lock().unwrap().push(push);
        let status = self.push_status.lock().unwrap().unwrap_or(201);
        Box::pin(async move { Ok(status) })
    }

    fn webhook(&self, post: WebhookPost) -> BoxFuture<'_, Result<u16, FederationError>> {
        self.webhook_posts.lock().unwrap().push(post);
        let status = self.webhook_status.lock().unwrap().unwrap_or(200);
        Box::pin(async move { Ok(status) })
    }

    fn service_request(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, Result<ServiceResponse, FederationError>> {
        let result = self
            .service_responses
            .lock()
            .unwrap()
            .get(&request.url)
            .cloned()
            .map(|(status, body)| ServiceResponse { status, body })
            .ok_or(FederationError::Status(404));
        self.service_requests.lock().unwrap().push(request);
        Box::pin(async move { result })
    }
}

pub fn test_config() -> Config {
    Config {
        domain: TEST_DOMAIN.to_owned(),
        account_domain: TEST_DOMAIN.to_owned(),
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
        encryption_secret: Some("test-encryption-secret-at-least-32-bytes".into()),
        encryption_secret_version: 1,
        encryption_previous_secrets: Vec::new(),
        altcha: plamenu::config::AltchaConfig {
            cost: 1,
            min_counter: 1,
            max_counter: 2,
            expires_seconds: 60,
        },
        update_check_url: None,
        translation: None,
        conversation_containers: false,
        csp_reporting: false,
        federation: plamenu::config::FederationConfig::default(),
    }
}

/// Flips the anonymous-access settings on (previews + public search): the
/// open posture tests exercising anonymous reads assume. Settings-owned since
/// migration 0013 (private by default), so call this *before* the app's first
/// request — the settings cache snapshots on first read.
pub async fn open_previews(pool: &PgPool) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        instance_settings::SettingsUpdate {
            timeline_preview_federated: true,
            timeline_preview_local: true,
            timeline_preview_tag: true,
            public_search: true,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

/// Pins the signature-emission flags off (settings-owned, default on) for
/// deterministic delivery assertions. Like [`open_previews`], call before the
/// state's first settings read.
pub async fn pin_emissions_off(pool: &PgPool) {
    let current = instance_settings::get(pool).await.unwrap();
    instance_settings::save(
        pool,
        instance_settings::SettingsUpdate {
            emit_integrity_proofs: false,
            emit_rfc9421: false,
            ..current.as_update()
        },
    )
    .await
    .unwrap();
}

pub fn test_state_with(pool: PgPool, federation: Arc<StubFederation>) -> AppState {
    AppState::new(
        pool,
        test_config(),
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}

/// Like [`test_state_with`] but with a caller-supplied media store, so a test
/// can inject a store that tracks how the serving path reads it (the archive
/// download resident-memory bound).
pub fn test_state_with_store(
    pool: PgPool,
    federation: Arc<StubFederation>,
    media: Arc<dyn plamenu::storage::MediaStore>,
) -> AppState {
    AppState::new(pool, test_config(), federation, media).unwrap()
}

/// Like [`test_state_with`] but with a remote-media cache retention period (in
/// days), for exercising eviction + redownload-on-demand. The period is a
/// live setting since migration 0013, so this saves it before the state's
/// first settings read.
pub async fn test_state_retention(
    pool: PgPool,
    federation: Arc<StubFederation>,
    retention_days: u64,
) -> AppState {
    let current = instance_settings::get(&pool).await.unwrap();
    instance_settings::save(
        &pool,
        instance_settings::SettingsUpdate {
            media_cache_retention_days: i32::try_from(retention_days).unwrap(),
            ..current.as_update()
        },
    )
    .await
    .unwrap();
    AppState::new(
        pool,
        test_config(),
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}

/// Like [`test_state_with`] but with outgoing e-mail configured (the mailer
/// worker is not running in tests, so flows just accumulate `email_jobs`
/// rows to assert on).
pub fn test_state_smtp(pool: PgPool, federation: Arc<StubFederation>) -> AppState {
    let config = Config {
        smtp: Some(SmtpConfig {
            server: "127.0.0.1".to_owned(),
            port: 1025,
            login: None,
            password: None,
            from_address: format!("notifications@{TEST_DOMAIN}"),
            ssl: false,
            starttls: Starttls::Never,
        }),
        ..test_config()
    };
    AppState::new(
        pool,
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}

/// An app that can send e-mail (registration flows require it).
pub fn test_app_smtp(pool: PgPool) -> Router {
    build_router(test_state_smtp(pool, Arc::default()))
}

/// Like [`test_state_with`] but with a translation backend configured (M25),
/// so the translate endpoint and `translation_languages` are live.
pub fn test_state_translation(
    pool: PgPool,
    federation: Arc<StubFederation>,
    translation: plamenu::config::TranslationConfig,
) -> AppState {
    let config = Config {
        translation: Some(translation),
        ..test_config()
    };
    AppState::new(
        pool,
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}

pub fn test_app(pool: PgPool) -> Router {
    test_app_with(pool, Arc::default())
}

/// A router in the conventional split-domain layout: Plamenu is hosted on
/// `host_domain`, while canonical account handles use `account_domain`.
pub fn test_app_split_domain(pool: PgPool, host_domain: &str, account_domain: &str) -> Router {
    let config = Config {
        domain: host_domain.to_owned(),
        account_domain: account_domain.to_owned(),
        ..test_config()
    };
    build_router(
        AppState::new(
            pool,
            config,
            Arc::new(StubFederation::default()),
            Arc::new(plamenu::storage::MemoryStore::default()),
        )
        .unwrap(),
    )
}

/// An app with the default-off CSP violation telemetry deliberately enabled.
pub fn test_app_csp_reporting(pool: PgPool) -> Router {
    let config = Config {
        csp_reporting: true,
        ..test_config()
    };
    build_router(
        AppState::new(
            pool,
            config,
            Arc::new(StubFederation::default()),
            Arc::new(plamenu::storage::MemoryStore::default()),
        )
        .unwrap(),
    )
}

pub fn test_app_with(pool: PgPool, federation: Arc<StubFederation>) -> Router {
    build_router(test_state_with(pool, federation))
}

/// A state/app with FEP-171b conversation containers enabled — the
/// dev/e2e/staging-with-flag posture. Off in the default config.
pub fn test_state_containers(pool: PgPool, federation: Arc<StubFederation>) -> AppState {
    let config = Config {
        conversation_containers: true,
        ..test_config()
    };
    AppState::new(
        pool,
        config,
        federation,
        Arc::new(plamenu::storage::MemoryStore::default()),
    )
    .unwrap()
}

pub fn test_app_containers(pool: PgPool) -> Router {
    build_router(test_state_containers(pool, Arc::default()))
}

/// An app in the private default: anonymous reads of every timeline type and
/// of search are denied. This *is* the settings default since migration 0013
/// — the helper exists so private-posture tests read as such (and don't call
/// [`open_previews`]).
pub fn test_app_private(pool: PgPool) -> Router {
    test_app(pool)
}

/// An app in authorized-fetch ("secure") mode: AP object GETs need a
/// valid signature.
pub fn test_app_secure(pool: PgPool, federation: Arc<StubFederation>) -> Router {
    let config = Config {
        authorized_fetch: true,
        ..test_config()
    };
    build_router(
        AppState::new(
            pool,
            config,
            federation,
            Arc::new(plamenu::storage::MemoryStore::default()),
        )
        .unwrap(),
    )
}

/// Secure mode with `authorized_fetch_unsigned = "profile"`: unsigned callers
/// get the profile document (Lemmy-interop posture) instead of the key-only
/// downgrade. The object routes stay gated exactly as in `test_app_secure`.
pub fn test_app_secure_unsigned_profile(pool: PgPool, federation: Arc<StubFederation>) -> Router {
    let config = Config {
        authorized_fetch: true,
        authorized_fetch_unsigned_profile: true,
        ..test_config()
    };
    build_router(
        AppState::new(
            pool,
            config,
            federation,
            Arc::new(plamenu::storage::MemoryStore::default()),
        )
        .unwrap(),
    )
}

/// Drains the delivery queue regardless of due time — interaction jobs
/// (`Like`/`Announce`/`EmojiReact`) sit out a short cancellation grace that
/// tests need not wait for. Returns how many jobs were delivered.
pub async fn deliver_all_due(state: &AppState) -> u64 {
    let mut total = 0;
    loop {
        plamenu_db::job::make_all_due(&state.pool).await.unwrap();
        let claimed = plamenu::delivery::run_due(state).await;
        if claimed == 0 {
            return total;
        }
        total += claimed;
    }
}

pub async fn create_local_account(pool: &PgPool, username: &str, display_name: &str) -> Account {
    let keypair = keys::generate_keypair().unwrap();
    let ed25519 = keys::generate_ed25519_keypair();
    let config = test_config();
    let keyring = plamenu::crypto::FederationKeyring::from_config(&config).unwrap();
    let mut tx = pool.begin().await.unwrap();
    let created = account::create_local_normalized_legacy(
        &mut *tx,
        NewLocalAccount {
            username,
            display_name,
            note: "test account",
            public_key_pem: &keypair.public_pem,
        },
    )
    .await
    .unwrap();
    plamenu::key_store::provision_account_tx(
        &mut tx,
        &keyring,
        TEST_DOMAIN,
        &created,
        &keypair,
        &ed25519,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    created
}

/// Production identity scheme: opaque numeric actor URI plus normalized,
/// encrypted RSA and Ed25519 keys committed in the same transaction.
pub async fn create_immutable_local_account(
    pool: &PgPool,
    username: &str,
    display_name: &str,
) -> Account {
    let keypair = keys::generate_keypair().unwrap();
    let ed25519 = keys::generate_ed25519_keypair();
    let config = test_config();
    let keyring = plamenu::crypto::FederationKeyring::from_config(&config).unwrap();
    let mut tx = pool.begin().await.unwrap();
    let created = account::create_local_immutable(
        &mut *tx,
        NewLocalAccount {
            username,
            display_name,
            note: "test account",
            public_key_pem: &keypair.public_pem,
        },
        TEST_DOMAIN,
    )
    .await
    .unwrap();
    plamenu::key_store::provision_account_tx(
        &mut tx,
        &keyring,
        TEST_DOMAIN,
        &created,
        &keypair,
        &ed25519,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    created
}

/// A fake remote user: an actor document plus the keys to sign as them.
pub struct RemoteUser {
    pub keys: KeyPairPem,
    /// FEP-521a Ed25519 pair, present after [`Self::with_ed25519`]; its
    /// public half is then published in the actor's `assertionMethod`.
    pub ed25519: Option<plamenu_ap::keys::Ed25519KeyPairMultibase>,
    pub actor: RemoteActor,
    pub acct: String,
}

impl RemoteUser {
    pub fn new(domain: &str, username: &str) -> Self {
        let keys = keys::generate_keypair().unwrap();
        let id = format!("https://{domain}/users/{username}");
        let actor = RemoteActor {
            kind: "Person".to_owned(),
            preferred_username: username.to_owned(),
            webfinger: None,
            inbox: format!("{id}/inbox"),
            followers: Some(serde_json::Value::String(format!("{id}/followers"))),
            following: Some(serde_json::Value::String(format!("{id}/following"))),
            outbox: Some(serde_json::Value::String(format!("{id}/outbox"))),
            name: None,
            summary: None,
            published: Some("2026-01-01T00:00:00Z".to_owned()),
            url: None,
            endpoints: Some(RemoteEndpoints {
                shared_inbox: Some(format!("https://{domain}/inbox")),
            }),
            icon: None,
            image: None,
            attachment: Vec::new(),
            tag: Vec::new(),
            featured: Some(serde_json::Value::String(format!(
                "{id}/collections/featured"
            ))),
            featured_tags: None,
            featured_collections: Some(serde_json::Value::String(format!(
                "{id}/featured_collections"
            ))),
            manually_approves_followers: false,
            discoverable: true,
            indexable: false,
            memorial: false,
            suspended: false,
            attribution_domains: Vec::new(),
            show_media: None,
            show_replies_in_media: None,
            show_featured: None,
            hides_to_public_from_unauthed_web: None,
            hides_cc_public_from_unauthed_web: None,
            interaction_policy: Some(serde_json::json!({
                "canFeature": {
                    "automaticApproval": ["https://www.w3.org/ns/activitystreams#Public"],
                },
            })),
            also_known_as: None,
            moved_to: None,
            public_key: PublicKey {
                id: format!("{id}#main-key"),
                owner: id.clone(),
                public_key_pem: keys.public_pem.clone(),
            }
            .into(),
            assertion_method: Vec::new(),
            implements: None,
            generator: None,
            // Community extensions: a plain remote person states none. Tests
            // that need a community build on top (`into_group`).
            sensitive: None,
            posting_restricted_to_mods: None,
            posting_policy: None,
            attributed_to: None,
            affiliations: None,
            id,
        };
        Self {
            keys,
            ed25519: None,
            actor,
            acct: format!("{username}@{domain}"),
        }
    }

    /// Gives the actor a FEP-521a Ed25519 key, published as a Multikey under
    /// `assertionMethod` the way Mitra does.
    #[must_use]
    pub fn with_ed25519(mut self) -> Self {
        let pair = plamenu_ap::keys::generate_ed25519_keypair();
        self.actor.assertion_method = vec![serde_json::json!({
            "id": self.ed25519_key_id(),
            "type": "Multikey",
            "controller": self.actor.id,
            "publicKeyMultibase": pair.public_multibase,
        })];
        self.ed25519 = Some(pair);
        self
    }

    pub fn ed25519_key_id(&self) -> String {
        format!("{}#ed25519-key", self.actor.id)
    }

    /// Attaches an FEP-8b32 `eddsa-jcs-2022` proof to `activity`, signed
    /// with this user's Ed25519 key.
    pub fn proof_signed(&self, activity: &serde_json::Value) -> serde_json::Value {
        plamenu_ap::proof::sign_document(
            activity,
            &self
                .ed25519
                .as_ref()
                .expect("call with_ed25519 first")
                .private_multibase,
            &self.ed25519_key_id(),
            "2026-07-11T00:00:00Z",
        )
        .unwrap()
    }

    pub fn signer(&self) -> RequestSigner {
        RequestSigner::from_pkcs8_pem(&self.keys.private_pem, self.actor.public_key.id.clone())
            .unwrap()
    }
}
