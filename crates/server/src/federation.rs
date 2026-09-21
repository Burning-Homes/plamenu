//! The server's view of the federation transport.
//!
//! A trait rather than direct `FederationClient` calls so integration tests
//! can exercise the complete inbox/outbox pipelines — signatures included —
//! without touching the network.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use plamenu_ap::acct::Acct;
use plamenu_ap::actor::RemoteActor;
use plamenu_federation::{
    FederationClient, FederationError, FetchedActivityPub, FetchedMedia, FetchedMediaFile,
    FetchedMediaRange, FetchedPage, HttpMethod, RequestSigner, ResolvedAcct, ServiceResponse,
    SignatureStyle,
};
use serde_json::Value;
use tokio::sync::Semaphore;
use url::Url;
use zeroize::Zeroizing;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An activity to deliver, signed by a local actor.
#[derive(Clone)]
pub struct Delivery {
    pub activity: Value,
    pub inbox_url: String,
    /// Extra headers to send with the POST. These are intentionally not part
    /// of the HTTP signature, matching Mastodon's delivery worker.
    pub headers: Vec<(String, String)>,
    /// PKCS#8 PEM of the signing (local) actor.
    pub private_key_pem: Zeroizing<String>,
    pub key_id: String,
    /// Attempt RFC 9421 first, double-knocking down to draft-cavage — set
    /// when emission is enabled and the host hasn't recently refused it.
    pub try_rfc9421: bool,
}

impl std::fmt::Debug for Delivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Delivery")
            .field("activity", &self.activity)
            .field("inbox_url", &self.inbox_url)
            .field("headers", &self.headers)
            .field("private_key_pem", &"[REDACTED]")
            .field("key_id", &self.key_id)
            .field("try_rfc9421", &self.try_rfc9421)
            .finish()
    }
}

/// An encrypted Web Push message bound for a push service endpoint.
#[derive(Debug, Clone)]
pub struct WebPush {
    pub endpoint: String,
    /// Scheme-specific headers (`Authorization`, `Content-Encoding`, …);
    /// `Content-Type: application/octet-stream` is implied.
    pub headers: Vec<(String, String)>,
    /// The encrypted payload.
    pub body: Vec<u8>,
}

/// A signed webhook payload bound for an admin-configured callback URL.
#[derive(Debug, Clone)]
pub struct WebhookPost {
    pub url: String,
    /// Extra headers (`X-Hub-Signature`); `Content-Type: application/json`
    /// is implied.
    pub headers: Vec<(String, String)>,
    /// The (possibly template-rendered) payload.
    pub body: String,
}

/// A plain GET/POST to an operator-configured translation backend
/// (`LibreTranslate` / `DeepL`), used by [`crate::translation`].
#[derive(Debug, Clone)]
pub struct ServiceRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Request body (`None` for a GET).
    pub body: Option<String>,
    /// Per-request timeout override in seconds; `None` keeps the client-wide
    /// default (15s). Slow self-hosted model backends need minutes.
    pub timeout_secs: Option<u64>,
}

pub trait FederationApi: Send + Sync {
    fn fetch_actor<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<RemoteActor, FederationError>>;
    /// Fetches an actor on behalf of one local account. Test doubles that do
    /// not model HTTP signatures inherit the ordinary fetch behavior.
    fn fetch_actor_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<RemoteActor, FederationError>> {
        let _ = account_id;
        self.fetch_actor(uri)
    }
    /// Fetches an arbitrary AP object (id-checked), e.g. a `QuoteAuthorization`.
    fn fetch_object<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<Value, FederationError>>;
    /// Fetches an object signed by the local account entitled to see it.
    fn fetch_object_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        let _ = account_id;
        self.fetch_object(uri)
    }
    /// Conditional signed AP fetch with transport metadata. Defaulted for test
    /// doubles; production overrides it to retain `ETag`, final URL, and bytes.
    fn fetch_activitypub<'a>(
        &'a self,
        uri: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchedActivityPub, FederationError>> {
        let _ = etag;
        Box::pin(async move {
            let document = self.fetch_object(uri).await?;
            let body_bytes = serde_json::to_vec(&document).map_or(0, |body| body.len());
            Ok(FetchedActivityPub {
                document: Some(document),
                etag: None,
                final_url: uri.to_owned(),
                body_bytes,
            })
        })
    }
    /// Fetches an AP object by a URL that may be a permalink, following to and
    /// validating the canonical `id` (see `FederationClient::fetch_object_following`).
    fn fetch_object_following<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value, FederationError>>;
    /// Permalink-following object fetch signed by one local account.
    fn fetch_object_following_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        let _ = account_id;
        self.fetch_object_following(uri)
    }
    /// [`Self::fetch_object_following`], but attempted even when the target's
    /// finite failure budget would refuse it — the federation-debug page needs
    /// the *underlying* error, which a pre-flight suppression never shows.
    /// Outcomes are still recorded, so a successful probe clears the budget (a
    /// manual unstick) and a failed one counts like any other attempt.
    /// Defaulted for test doubles, which track no budgets.
    fn fetch_object_following_ignoring_budget<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        self.fetch_object_following(uri)
    }
    /// Fetches a web page or oEmbed document (for link previews), following
    /// redirects with the SSRF guard applied to every hop.
    fn fetch_page<'a>(
        &'a self,
        url: &'a str,
        accept: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FederationError>>;
    /// Downloads a remote media file for local caching (SSRF-guarded per
    /// redirect hop, size-capped). See [`FederationClient::fetch_media`].
    fn fetch_media<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>>;
    /// Fetches binary content under a caller-selected exact byte ceiling.
    /// Test doubles inherit a checked in-memory implementation; production
    /// streams into a scratch file so an oversized origin is stopped before
    /// it can consume the general 99 MiB media allowance.
    fn fetch_media_limited<'a>(
        &'a self,
        url: &'a str,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>> {
        Box::pin(async move {
            let fetched = self.fetch_media(url).await?;
            if u64::try_from(fetched.bytes.len()).unwrap_or(u64::MAX) > max_bytes {
                return Err(FederationError::TooLarge(url.to_owned()));
            }
            Ok(fetched)
        })
    }
    /// Downloads a large remote media file streamed to `dest`, capped at
    /// `max_bytes` (remote video caching). See
    /// [`FederationClient::fetch_media_to_file`]. Defaulted so test doubles
    /// that never exercise video caching need no stub.
    fn fetch_media_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaFile, FederationError>> {
        let _ = (url, dest, max_bytes);
        Box::pin(async { Err(FederationError::Status(404)) })
    }
    /// Fetches one byte range of a remote file, streamed to `dest` (the caching
    /// HLS reverse-proxy's segment fetch). See
    /// [`FederationClient::fetch_range_to_file`]. Defaulted so test doubles that
    /// never exercise HLS playback need no stub.
    fn fetch_range_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<u64, FederationError>> {
        let _ = (url, dest, start, len);
        Box::pin(async { Err(FederationError::Status(404)) })
    }
    /// Opens an exact, SSRF-checked byte range as a cancellable stream. The
    /// returned body owns the network request: dropping it stops origin work.
    fn fetch_media_range<'a>(
        &'a self,
        url: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaRange, FederationError>> {
        let _ = (url, start, len);
        Box::pin(async { Err(FederationError::Status(404)) })
    }
    fn resolve_acct<'a>(
        &'a self,
        acct: &'a Acct,
    ) -> BoxFuture<'a, Result<ResolvedAcct, FederationError>>;
    /// Delivers a signed activity, reporting which signature dialect the
    /// receiving host ended up accepting (for the double-knock memory).
    fn deliver(&self, delivery: Delivery)
    -> BoxFuture<'_, Result<SignatureStyle, FederationError>>;
    /// Sends a Web Push message, returning the push service's HTTP status.
    fn web_push(&self, push: WebPush) -> BoxFuture<'_, Result<u16, FederationError>>;
    /// POSTs a webhook payload, returning the receiver's HTTP status.
    fn webhook(&self, post: WebhookPost) -> BoxFuture<'_, Result<u16, FederationError>>;
    /// Performs a plain request to the translation backend, returning its
    /// status and body for the caller to parse.
    fn service_request(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, Result<ServiceResponse, FederationError>>;
}

/// The real implementation, backed by [`FederationClient`]. Remote fetches
/// pass through a persistent finite failure budget and a global admission
/// limit before touching the network.
pub struct HttpFederation {
    client: FederationClient,
    pool: plamenu_db::PgPool,
    local_domain: String,
    keyring: Arc<crate::crypto::FederationKeyring>,
    fetch_permits: Arc<Semaphore>,
}

const MAX_CONCURRENT_REMOTE_FETCHES: usize = 32;

impl HttpFederation {
    #[must_use]
    pub fn new(
        client: FederationClient,
        pool: plamenu_db::PgPool,
        local_domain: String,
        keyring: Arc<crate::crypto::FederationKeyring>,
    ) -> Self {
        Self {
            client,
            pool,
            local_domain,
            keyring,
            fetch_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REMOTE_FETCHES)),
        }
    }

    async fn account_fetch_signer(
        &self,
        account_id: i64,
    ) -> Result<RequestSigner, FederationError> {
        let records = plamenu_db::actor_key::usable_for_account(&self.pool, account_id)
            .await
            .map_err(|error| FederationError::Admission(error.to_string()))?;
        let rsa_record = records
            .iter()
            .find(|key| key.algorithm == "rsa" && key.encrypted_private_key.is_some())
            .cloned()
            .ok_or_else(|| {
                FederationError::Admission(format!(
                    "local account {account_id} has no usable RSA signing key"
                ))
            })?;
        let rsa = crate::key_store::decrypt_record(&self.keyring, rsa_record)
            .map_err(|error| FederationError::Admission(error.to_string()))?;
        let mut signer = RequestSigner::from_pkcs8_pem(
            rsa.private
                .expose_str()
                .map_err(|error| FederationError::Admission(error.to_string()))?,
            rsa.record.key_uri,
        )?;
        if let Some(ed_record) = records
            .into_iter()
            .find(|key| key.algorithm == "ed25519" && key.encrypted_private_key.is_some())
        {
            let ed = crate::key_store::decrypt_record(&self.keyring, ed_record)
                .map_err(|error| FederationError::Admission(error.to_string()))?;
            signer = signer.with_ed25519(
                ed.record.key_uri,
                ed.private
                    .expose_str()
                    .map_err(|error| FederationError::Admission(error.to_string()))?,
            );
        }
        Ok(signer)
    }

    async fn guarded<T>(
        &self,
        target: &str,
        request: impl Future<Output = Result<T, FederationError>>,
    ) -> Result<T, FederationError> {
        self.guarded_inner(target, "resource-instance", target, request, false)
            .await
    }

    async fn guarded_for_account<T>(
        &self,
        target: &str,
        account_id: i64,
        request: impl Future<Output = Result<T, FederationError>>,
    ) -> Result<T, FederationError> {
        // Authorization failures are actor-relative. Keep their finite retry
        // budget separate from the instance signer and every other account,
        // so an earlier 403 cannot suppress an entitled recipient's fetch.
        let resource_key = format!("{account_id}:{target}");
        self.guarded_inner(target, "resource-account", &resource_key, request, false)
            .await
    }

    /// [`Self::guarded`] minus the pre-flight budget refusal: admission
    /// permits, the hidden-service gate and outcome recording all still apply,
    /// so the probe behaves exactly like a fetch the budget would have
    /// allowed.
    async fn guarded_ignoring_budget<T>(
        &self,
        target: &str,
        request: impl Future<Output = Result<T, FederationError>>,
    ) -> Result<T, FederationError> {
        self.guarded_inner(target, "resource-instance", target, request, true)
            .await
    }

    async fn guarded_inner<T>(
        &self,
        target: &str,
        authorization_scope: &str,
        authorization_key: &str,
        request: impl Future<Output = Result<T, FederationError>>,
        ignore_budget: bool,
    ) -> Result<T, FederationError> {
        // Hidden-service fetches serialize on their own one-permit gate,
        // acquired *before* an admission permit so the queue waiting for the
        // overlay network cannot occupy the clearnet pool's slots.
        let _hidden_permit = crate::hidden_gate::fetch_permit(target).await;
        let _permit = self
            .fetch_permits
            .acquire()
            .await
            .map_err(|error| FederationError::Admission(error.to_string()))?;
        let host = Url::parse(target)
            .ok()
            .and_then(|url| url.host_str().map(str::to_lowercase));
        if !ignore_budget {
            // Objective failures (404, malformed content, and so on) stay
            // globally bounded, while authorization failures are checked for
            // the signer that received them.
            if !plamenu_db::remote_fetch_failure::should_attempt(&self.pool, "resource", target)
                .await
                .map_err(|error| FederationError::Admission(error.to_string()))?
                || !plamenu_db::remote_fetch_failure::should_attempt(
                    &self.pool,
                    authorization_scope,
                    authorization_key,
                )
                .await
                .map_err(|error| FederationError::Admission(error.to_string()))?
            {
                return Err(FederationError::FetchSuppressed(target.to_owned()));
            }
            if let Some(host) = &host
                && !plamenu_db::remote_fetch_failure::should_attempt(&self.pool, "host", host)
                    .await
                    .map_err(|error| FederationError::Admission(error.to_string()))?
            {
                return Err(FederationError::FetchSuppressed(host.clone()));
            }
        }

        match request.await {
            Ok(value) => {
                let _ =
                    plamenu_db::remote_fetch_failure::clear(&self.pool, "resource", target).await;
                let _ = plamenu_db::remote_fetch_failure::clear(
                    &self.pool,
                    authorization_scope,
                    authorization_key,
                )
                .await;
                if let Some(host) = &host {
                    let _ = plamenu_db::remote_fetch_failure::clear(&self.pool, "host", host).await;
                    let _ = plamenu_db::reachability::record_success(&self.pool, host).await;
                }
                Ok(value)
            }
            Err(error) => {
                if let FederationError::RateLimited { retry_after_secs } = &error {
                    let seconds = retry_after_secs.unwrap_or(60).clamp(1, 86_400);
                    let message = error.to_string();
                    let _ = plamenu_db::remote_fetch_failure::record_backoff(
                        &self.pool, "resource", target, seconds, &message,
                    )
                    .await;
                    if let Some(host) = &host {
                        let _ = plamenu_db::remote_fetch_failure::record_backoff(
                            &self.pool, "host", host, seconds, &message,
                        )
                        .await;
                    }
                    return Err(error);
                }
                if counts_as_resource_failure(&error) {
                    // Authorized-fetch servers commonly privacy-mask an
                    // existing protected object as 404. Like an explicit
                    // 401/403, that answer is true only for this signer; an
                    // entitled local actor must still get its own attempt.
                    let (scope, key) = if matches!(error, FederationError::Status(401 | 403 | 404))
                    {
                        (authorization_scope, authorization_key)
                    } else {
                        ("resource", target)
                    };
                    let _ = plamenu_db::remote_fetch_failure::record_failure(
                        &self.pool,
                        scope,
                        key,
                        &error.to_string(),
                    )
                    .await;
                }
                if let Some(host) = &host
                    && counts_as_host_failure_for(&error, host)
                {
                    let _ = plamenu_db::remote_fetch_failure::record_failure(
                        &self.pool,
                        "host",
                        host,
                        &error.to_string(),
                    )
                    .await;
                }
                Err(error)
            }
        }
    }
}

fn counts_as_resource_failure(error: &FederationError) -> bool {
    !matches!(
        error,
        FederationError::InvalidUrl(_)
            | FederationError::PrivateAddress(_)
            | FederationError::Signing(_)
            | FederationError::Io(_)
            | FederationError::FetchSuppressed(_)
            | FederationError::Admission(_)
    )
}

fn counts_as_host_failure(error: &FederationError) -> bool {
    matches!(
        error,
        FederationError::Http(_) | FederationError::Stalled(_) | FederationError::Status(500..=599)
    )
}

/// [`counts_as_host_failure`], but transport-level failures toward a
/// hidden-service host are *not* held against it: they reach the host through
/// a Tor/I2P circuit, and a failed circuit is our infrastructure misbehaving,
/// not theirs (plan §7.1 — otherwise onion hosts get budget-suppressed by
/// ordinary Tor churn). An HTTP status means the host itself answered, so
/// 5xx still counts.
fn counts_as_host_failure_for(error: &FederationError, host: &str) -> bool {
    if plamenu_federation::is_hidden_service(host) {
        return matches!(error, FederationError::Status(500..=599));
    }
    counts_as_host_failure(error)
}

impl FederationApi for HttpFederation {
    fn fetch_actor<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<RemoteActor, FederationError>> {
        Box::pin(self.guarded(uri, self.client.fetch_actor(uri)))
    }

    fn fetch_actor_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<RemoteActor, FederationError>> {
        Box::pin(async move {
            let signer = self.account_fetch_signer(account_id).await?;
            self.guarded_for_account(
                uri,
                account_id,
                self.client.fetch_actor_as(uri, Some(&signer)),
            )
            .await
        })
    }

    fn fetch_object<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<Value, FederationError>> {
        Box::pin(self.guarded(uri, self.client.fetch_object(uri)))
    }

    fn fetch_object_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        Box::pin(async move {
            let signer = self.account_fetch_signer(account_id).await?;
            self.guarded_for_account(
                uri,
                account_id,
                self.client.fetch_object_as(uri, Some(&signer)),
            )
            .await
        })
    }

    fn fetch_activitypub<'a>(
        &'a self,
        uri: &'a str,
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<FetchedActivityPub, FederationError>> {
        Box::pin(
            self.guarded(
                uri,
                self.client
                    .fetch_activitypub_with_redirect_guard(uri, etag, move |next| {
                        let next = next.to_owned();
                        async move {
                            crate::instance_policy::can_federate_url(
                                &self.pool,
                                &self.local_domain,
                                &next,
                            )
                            .await
                            .map_err(|error| FederationError::Admission(error.to_string()))
                        }
                    }),
            ),
        )
    }

    fn fetch_object_following<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        Box::pin(self.guarded(uri, self.client.fetch_object_following(uri)))
    }

    fn fetch_object_following_for_account<'a>(
        &'a self,
        uri: &'a str,
        account_id: i64,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        Box::pin(async move {
            let signer = self.account_fetch_signer(account_id).await?;
            self.guarded_for_account(
                uri,
                account_id,
                self.client.fetch_object_following_as(uri, Some(&signer)),
            )
            .await
        })
    }

    fn fetch_object_following_ignoring_budget<'a>(
        &'a self,
        uri: &'a str,
    ) -> BoxFuture<'a, Result<Value, FederationError>> {
        Box::pin(self.guarded_ignoring_budget(uri, self.client.fetch_object_following(uri)))
    }

    fn fetch_page<'a>(
        &'a self,
        url: &'a str,
        accept: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FederationError>> {
        Box::pin(self.guarded(url, self.client.fetch_page(url, accept)))
    }

    fn fetch_media<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>> {
        Box::pin(self.guarded(url, self.client.fetch_media(url)))
    }

    fn fetch_media_limited<'a>(
        &'a self,
        url: &'a str,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<FetchedMedia, FederationError>> {
        Box::pin(async move {
            let scratch = tempfile::tempdir()?;
            let path = scratch.path().join("download");
            let fetched = self
                .guarded(url, self.client.fetch_media_to_file(url, &path, max_bytes))
                .await?;
            let bytes = tokio::fs::read(path).await?;
            Ok(FetchedMedia {
                final_url: fetched.final_url,
                content_type: fetched.content_type,
                bytes,
            })
        })
    }

    fn fetch_media_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaFile, FederationError>> {
        Box::pin(self.guarded(url, self.client.fetch_media_to_file(url, dest, max_bytes)))
    }

    fn fetch_range_to_file<'a>(
        &'a self,
        url: &'a str,
        dest: &'a std::path::Path,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<u64, FederationError>> {
        Box::pin(self.guarded(url, self.client.fetch_range_to_file(url, dest, start, len)))
    }

    fn fetch_media_range<'a>(
        &'a self,
        url: &'a str,
        start: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<FetchedMediaRange, FederationError>> {
        Box::pin(async move {
            // Keep the permits for the lifetime of the response body, not
            // merely until its headers arrive. Hidden gate first, same order
            // and reason as `guarded`.
            let hidden_permit = crate::hidden_gate::fetch_permit(url).await;
            let permit = self
                .fetch_permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|error| FederationError::Admission(error.to_string()))?;
            let host = Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_lowercase));
            if !plamenu_db::remote_fetch_failure::should_attempt(&self.pool, "resource", url)
                .await
                .map_err(|error| FederationError::Admission(error.to_string()))?
            {
                return Err(FederationError::FetchSuppressed(url.to_owned()));
            }
            if let Some(host) = &host
                && !plamenu_db::remote_fetch_failure::should_attempt(&self.pool, "host", host)
                    .await
                    .map_err(|error| FederationError::Admission(error.to_string()))?
            {
                return Err(FederationError::FetchSuppressed(host.clone()));
            }
            let fetched = match self.client.fetch_media_range(url, start, len).await {
                Ok(fetched) => fetched,
                Err(error) => {
                    if counts_as_resource_failure(&error) {
                        let _ = plamenu_db::remote_fetch_failure::record_failure(
                            &self.pool,
                            "resource",
                            url,
                            &error.to_string(),
                        )
                        .await;
                    }
                    if let Some(host) = &host
                        && counts_as_host_failure_for(&error, host)
                    {
                        let _ = plamenu_db::remote_fetch_failure::record_failure(
                            &self.pool,
                            "host",
                            host,
                            &error.to_string(),
                        )
                        .await;
                    }
                    return Err(error);
                }
            };
            let _ = plamenu_db::remote_fetch_failure::clear(&self.pool, "resource", url).await;
            if let Some(host) = &host {
                let _ = plamenu_db::remote_fetch_failure::clear(&self.pool, "host", host).await;
                let _ = plamenu_db::reachability::record_success(&self.pool, host).await;
            }
            let FetchedMediaRange {
                final_url,
                content_type,
                total_len,
                start,
                len,
                bytes,
            } = fetched;
            let bytes = stream::unfold(
                (bytes, permit, hidden_permit),
                |(mut bytes, permit, hidden_permit)| async move {
                    bytes
                        .next()
                        .await
                        .map(|item| (item, (bytes, permit, hidden_permit)))
                },
            );
            Ok(FetchedMediaRange {
                final_url,
                content_type,
                total_len,
                start,
                len,
                bytes: Box::pin(bytes),
            })
        })
    }

    fn resolve_acct<'a>(
        &'a self,
        acct: &'a Acct,
    ) -> BoxFuture<'a, Result<ResolvedAcct, FederationError>> {
        let target = format!("https://{}/.well-known/webfinger", acct.domain());
        Box::pin(async move { self.guarded(&target, self.client.resolve_acct(acct)).await })
    }

    fn deliver(
        &self,
        delivery: Delivery,
    ) -> BoxFuture<'_, Result<SignatureStyle, FederationError>> {
        Box::pin(async move {
            let signer =
                RequestSigner::from_pkcs8_pem(&delivery.private_key_pem, delivery.key_id.clone())?;
            // At most one hidden-service delivery in flight at a time,
            // however the delivery worker fans out (Mitra: simultaneous
            // onion requests frequently fail).
            let _hidden_permit = crate::hidden_gate::delivery_permit(&delivery.inbox_url).await;
            self.client
                .deliver(
                    &delivery.activity,
                    &delivery.inbox_url,
                    &signer,
                    &delivery.headers,
                    delivery.try_rfc9421,
                )
                .await
        })
    }

    fn web_push(&self, push: WebPush) -> BoxFuture<'_, Result<u16, FederationError>> {
        Box::pin(async move {
            self.client
                .web_push(&push.endpoint, &push.headers, push.body)
                .await
        })
    }

    fn webhook(&self, post: WebhookPost) -> BoxFuture<'_, Result<u16, FederationError>> {
        Box::pin(async move {
            self.client
                .webhook_post(&post.url, &post.headers, post.body)
                .await
        })
    }

    fn service_request(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, Result<ServiceResponse, FederationError>> {
        Box::pin(async move {
            self.client
                .service_request(
                    request.method,
                    &request.url,
                    &request.headers,
                    request.body,
                    request.timeout_secs,
                )
                .await
        })
    }
}
