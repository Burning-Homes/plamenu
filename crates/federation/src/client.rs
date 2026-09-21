//! Outbound federation HTTP: actor dereferencing, webfinger resolution and
//! signed delivery. Every request passes the SSRF [`crate::guard`] first.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use futures_util::{Stream, stream};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use plamenu_ap::acct::Acct;
use plamenu_ap::actor::RemoteActor;
use plamenu_ap::webfinger::Jrd;
use plamenu_ap::{ACTIVITY_JSON, LD_JSON_AS};
use reqwest::Url;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, ETAG, HeaderMap, IF_NONE_MATCH, LINK, RETRY_AFTER,
    USER_AGENT,
};
use serde_json::Value;
use thiserror::Error;

use crate::guard;
use crate::signature::RequestSigner;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Request timeout for hidden-service destinations. An onion round trip
/// crosses a six-hop overlay circuit; holding it to the clearnet 15 s budget
/// would churn the delivery retry queue on requests that were going to
/// succeed.
#[allow(
    clippy::duration_suboptimal_units,
    reason = "from_mins is still behind nightly duration_constructors"
)]
const HIDDEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Hard ceiling for `ActivityPub` and `WebFinger` JSON documents. These are
/// metadata, not media; accepting an arbitrarily large document lets a fast
/// hostile peer exhaust the process before the request timeout fires.
const FEDERATION_JSON_LIMIT: usize = 2 * 1024 * 1024;

/// Mastodon's `FetchResourceService` Accept header: prefer `ActivityPub`, but
/// allow an HTML response so we can discover an advertised AP alternate.
const RESOURCE_ACCEPT: &str = "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\", application/activity+json, text/html;q=0.1";

/// How much of a fetched page body is kept (Mastodon truncates at 1 MiB
/// too) — link-preview metadata lives in `<head>`, far before this.
const PAGE_BODY_LIMIT: usize = 1 << 20;

/// How many redirects a page fetch follows, each hop re-checked by the
/// SSRF guard.
const PAGE_MAX_REDIRECTS: usize = 3;

/// `Accept-Language` for page fetches, Mastodon's
/// `"#{I18n.default_locale}, *;q=0.5"` — without it, geo-localizing sites
/// answer in the server's datacenter locale and the card comes back in the
/// wrong language.
const PAGE_ACCEPT_LANGUAGE: &str = "en, *;q=0.5";

/// How many redirects signed delivery follows, re-signing and re-checking
/// every hop.
const DELIVERY_MAX_REDIRECTS: usize = 3;

/// Hard cap on a downloaded media body — Mastodon's `VIDEO_LIMIT` (99 MiB),
/// the largest upload it accepts. Oversized downloads fail rather than
/// truncate: a partial media file is unusable.
const MEDIA_BODY_LIMIT: usize = 99 * 1024 * 1024;

/// Explicit media negotiation used for authenticated downloads. Funkwhale's
/// draft-cavage verifier checks every header named by the signature, so this
/// value must be sent verbatim when it is covered by [`RequestSigner::sign_get`].
const MEDIA_ACCEPT: &str = "*/*";

/// Ceiling on one large-media download's total wall time (the caller's byte
/// cap is the real limit; this only reaps a download that would otherwise
/// crawl forever). Overrides the client-wide 15 s request timeout, which a
/// large video could never meet. Five minutes is also the fairness budget for
/// the single A/V lane: a trickling origin must yield to the next queued item.
#[allow(
    clippy::duration_suboptimal_units,
    reason = "from_mins is still behind nightly duration_constructors"
)]
const AV_FETCH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a large-media download tolerates no bytes arriving at all before
/// it is declared stalled.
const AV_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock ceiling on ONE HLS segment fetch (a few seconds of video, KB–MB).
/// Unlike a whole-file download this must finish quickly; a slow or half-dead
/// origin is bounded here rather than tying up a request handler for the five minutes
/// [`AV_FETCH_TIMEOUT`] allows.
const SEGMENT_FETCH_TIMEOUT: Duration = Duration::from_secs(45);

/// Formats an error with its full source chain. reqwest's own `Display` stops
/// at "error sending request", which buries the resolver/connector cause that
/// a federation log line or a recorded `last_error` actually needs.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    use std::fmt::Write;
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let _ = write!(out, ": {cause}");
        source = cause.source();
    }
    out
}

fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or_default()
            .as_secs(),
    )
}

#[derive(Debug, Error)]
pub enum FederationError {
    #[error("http request failed: {}", error_chain(.0))]
    Http(#[from] reqwest::Error),
    #[error("remote answered {0}")]
    Status(u16),
    #[error("remote answered 429 (retry after {retry_after_secs:?}s)")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("{0} is larger than the media download limit")]
    TooLarge(String),
    #[error("{0} is larger than the federation document limit")]
    DocumentTooLarge(String),
    #[error("{0}: download stalled")]
    Stalled(String),
    #[error("remote fetch suppressed by its finite failure budget: {0}")]
    FetchSuppressed(String),
    #[error("remote fetch admission failed: {0}")]
    Admission(String),
    #[error("local i/o failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} resolves to a private address")]
    PrivateAddress(String),
    #[error("invalid actor document: {0}")]
    InvalidActor(String),
    #[error("webfinger gave no usable self link for {0}")]
    NoSelfLink(String),
    #[error("signing failed: {0}")]
    Signing(#[from] crate::signature::SignatureError),
}

/// A fetched web page (or oEmbed document): the URL after redirects, the
/// `Content-Type` header and the (possibly truncated) decoded body.
#[derive(Debug, Clone)]
pub struct FetchedPage {
    pub final_url: String,
    pub content_type: String,
    pub body: String,
}

/// One conditional, signed `ActivityPub` document fetch. `document` is `None`
/// only for `304 Not Modified`; byte accounting is the decoded response size,
/// before JSON parsing.
#[derive(Debug, Clone)]
pub struct FetchedActivityPub {
    pub document: Option<Value>,
    pub etag: Option<String>,
    pub final_url: String,
    pub body_bytes: usize,
}

/// HTTP verb for a plain [`FederationClient::service_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// The result of a [`FederationClient::service_request`]: the HTTP status and
/// the (possibly truncated) decoded response body.
#[derive(Debug, Clone)]
pub struct ServiceResponse {
    pub status: u16,
    pub body: String,
}

/// A downloaded binary resource (remote media — an attachment, avatar or
/// header): the URL after redirects, the `Content-Type` header and the raw
/// bytes.
#[derive(Debug, Clone)]
pub struct FetchedMedia {
    pub final_url: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A large media download streamed to disk (remote video caching): the URL
/// after redirects, the `Content-Type` header and the byte count written to
/// the destination file.
#[derive(Debug, Clone)]
pub struct FetchedMediaFile {
    pub final_url: String,
    pub content_type: String,
    pub bytes_written: u64,
}

/// A validated byte-range response whose body remains streaming.  Dropping
/// `bytes` drops reqwest's response body and therefore cancels the upstream
/// transfer; playback routes use that property to bind origin work to the
/// downstream viewer instead of a detached background job.
pub struct FetchedMediaRange {
    pub final_url: String,
    pub content_type: String,
    pub total_len: u64,
    pub start: u64,
    pub len: u64,
    pub bytes: Pin<Box<dyn Stream<Item = Result<Vec<u8>, FederationError>> + Send>>,
}

/// One `self` link from a `WebFinger` JRD: the actor URI it names and the
/// `ActivityStreams` `type` it advertises (via RFC 7033 `properties`). The type
/// is a *hint* — the fetched actor document is authoritative — but it lets a
/// resolver tell a Person `self` link from a Group one when a host (Lemmy)
/// answers a single `acct` with both.
#[derive(Debug, Clone)]
pub struct WebfingerCandidate {
    pub actor_uri: String,
    pub advertised_type: Option<String>,
}

impl WebfingerCandidate {
    /// Whether this candidate advertises a `Group` actor. Person/Service/absent
    /// all read as not-a-group.
    #[must_use]
    pub fn is_group_hint(&self) -> bool {
        self.advertised_type.as_deref() == Some("Group")
    }
}

/// The result of resolving an acct URI through `WebFinger`: the confirmed acct
/// subject and every `ActivityPub` actor it advertises. A handle usually names
/// one actor, but an FEP-1b12 host (Lemmy) serves a Person and a Group under one
/// `acct`, so all candidates are preserved for typed resolution.
#[derive(Debug, Clone)]
pub struct ResolvedAcct {
    pub acct: Acct,
    /// The Mastodon-compatible single pick for callers that consume one URI:
    /// the first person-like candidate, else the first candidate (so a
    /// Group-only handle still resolves to its Group).
    pub actor_uri: String,
    /// Every advertised actor for this handle, in JRD order, deduplicated by
    /// URI. `actor_uri` is always one of these.
    pub candidates: Vec<WebfingerCandidate>,
    /// The `http://ostatus.org/schema/1.0/subscribe` link template, when the
    /// home server advertises one — the remote-interaction interstitial
    /// substitutes `{uri}` and sends the visitor there.
    pub subscribe_template: Option<String>,
    /// An HLS stream advertised by the account's `WebFinger` document. `Owncast`
    /// publishes its stable `/hls/stream.m3u8` endpoint this way; ordinary
    /// `ActivityPub` implementations omit it.
    pub hls_stream_url: Option<String>,
}

impl ResolvedAcct {
    /// A single-actor result (the common Mastodon/Pleroma case).
    #[must_use]
    pub fn single(acct: Acct, actor_uri: String) -> Self {
        Self {
            acct,
            candidates: vec![WebfingerCandidate {
                actor_uri: actor_uri.clone(),
                advertised_type: None,
            }],
            actor_uri,
            subscribe_template: None,
            hls_stream_url: None,
        }
    }
}

/// Outbound proxy routing — the per-destination transport seam (Mitra's
/// `proxy_url` / `onion_proxy_url` / `i2p_proxy_url` / `no_proxy` shape).
/// Only `socks5h://` proxies make sense for hidden services: the `h` defers
/// hostname resolution to the proxy, and `.onion`/`.i2p` names never resolve
/// via DNS (verified against reqwest 0.13: `socks5://` resolves locally and
/// fails). HTTP proxies also work for operators running a Privoxy-style shim.
#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    /// Routes all outbound federation traffic unless a more specific lane
    /// applies.
    pub proxy_url: Option<String>,
    /// Routes `.onion` destinations; falls back to `proxy_url`.
    pub onion_proxy_url: Option<String>,
    /// Routes `.i2p` destinations; falls back to `proxy_url`.
    pub i2p_proxy_url: Option<String>,
    /// Hosts that bypass every proxy. Matched against the exact URL host,
    /// case-insensitively — no suffix or wildcard matching.
    pub no_proxy: Vec<String>,
}

pub struct FederationClient {
    http: reqwest::Client,
    /// Like `http` but never follows redirects — page fetches walk each hop
    /// manually so the SSRF guard sees every redirect target.
    http_no_redirect: reqwest::Client,
    /// Unproxied client, built only when a global `proxy_url` exists (without
    /// one, `http` already dials directly): serves `no_proxy` hosts and
    /// operator-pointed infrastructure (webhooks, the translation backend).
    direct: Option<reqwest::Client>,
    /// Client for `.onion` destinations, built when an onion-capable proxy is
    /// configured. Longer timeout: Tor round trips are six hops.
    onion: Option<reqwest::Client>,
    /// Client for `.i2p` destinations, likewise.
    i2p: Option<reqwest::Client>,
    /// Hosts that bypass every proxy (exact, case-insensitive host match).
    no_proxy: Vec<String>,
    /// Kept so `with_proxies` can build the extra lanes after construction.
    user_agent: String,
    /// Waives the private-address guard; only for closed test environments
    /// like the local Mastodon stack.
    allow_private: bool,
    /// Signs every object/actor GET (with the instance actor's key), so
    /// remotes running authorized-fetch ("secure mode") answer us. Webfinger
    /// stays unsigned, like Mastodon's.
    fetch_signer: Option<RequestSigner>,
    /// Overrides the `User-Agent` on page fetches only (link previews) —
    /// crawler-gating sites serve their metadata to known fetchers, so this
    /// carries the Mastodon-compat marker and a `Bot` suffix.
    page_user_agent: Option<String>,
}

struct DeliveryResponse {
    status: reqwest::StatusCode,
    location: Option<String>,
    /// Parsed `Retry-After` (seconds form only), kept from 429 responses so
    /// the delivery queue can honor the server's own pacing.
    retry_after_secs: Option<u64>,
}

/// Which HTTP-signature dialect a delivery went out with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureStyle {
    /// draft-cavage `Signature` header — what everything speaks.
    Cavage,
    /// RFC 9421 `Signature-Input`/`Signature` — Mastodon ≥4.4 and Mitra.
    Rfc9421,
}

enum ResourceHop {
    Object(Value),
    Alternate(String),
}

impl FederationClient {
    pub fn new(user_agent: &str, allow_private: bool) -> Result<Self, FederationError> {
        let http = build_client(user_agent, allow_private, None, REQUEST_TIMEOUT)?;
        let http_no_redirect = build_client(user_agent, allow_private, None, REQUEST_TIMEOUT)?;
        Ok(Self {
            http,
            http_no_redirect,
            direct: None,
            onion: None,
            i2p: None,
            no_proxy: Vec::new(),
            user_agent: user_agent.to_owned(),
            allow_private,
            fetch_signer: None,
            page_user_agent: None,
        })
    }

    /// Installs per-destination proxy routing. The `SafeResolver` stays on
    /// every lane exactly as before — it is dormant on `socks5h` requests
    /// (the proxy resolves), and load-bearing on direct ones.
    pub fn with_proxies(mut self, proxies: &ProxyConfig) -> Result<Self, FederationError> {
        if let Some(global) = proxies.proxy_url.as_deref() {
            let via_global = build_client(
                &self.user_agent,
                self.allow_private,
                Some(global),
                REQUEST_TIMEOUT,
            )?;
            // `http` and `http_no_redirect` are built identically (redirects
            // are walked manually everywhere), so one proxied pool serves
            // both names.
            self.http = via_global.clone();
            self.http_no_redirect = via_global;
            self.direct = Some(build_client(
                &self.user_agent,
                self.allow_private,
                None,
                REQUEST_TIMEOUT,
            )?);
        }
        for (lane, proxy_url) in [
            (&mut self.onion, proxies.onion_proxy_url.as_deref()),
            (&mut self.i2p, proxies.i2p_proxy_url.as_deref()),
        ] {
            // Per-network override, else the global proxy. With neither, the
            // lane stays unbuilt and hidden-service requests fail cleanly at
            // resolution.
            let Some(proxy) = proxy_url.or(proxies.proxy_url.as_deref()) else {
                continue;
            };
            *lane = Some(build_client(
                &self.user_agent,
                self.allow_private,
                Some(proxy),
                HIDDEN_REQUEST_TIMEOUT,
            )?);
        }
        self.no_proxy.clone_from(&proxies.no_proxy);
        Ok(self)
    }

    /// The client a request to `url` must use: its overlay network's lane,
    /// with `no_proxy` short-circuiting to the unproxied client.
    fn http_for(&self, url: &Url) -> &reqwest::Client {
        self.lane_for(url).unwrap_or(&self.http)
    }

    /// [`Self::http_for`] for call sites that used `http_no_redirect`. The
    /// special lanes are shared: every lane has redirects off and redirect
    /// hops are walked manually everywhere.
    fn http_no_redirect_for(&self, url: &Url) -> &reqwest::Client {
        self.lane_for(url).unwrap_or(&self.http_no_redirect)
    }

    fn lane_for(&self, url: &Url) -> Option<&reqwest::Client> {
        let host = url.host_str().unwrap_or_default();
        if self
            .no_proxy
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(host))
        {
            // `direct` is only built when a global proxy exists; without one
            // the default lane already dials directly. (A hidden-service host
            // listed here fails by construction — it needs its proxy.)
            return Some(self.direct.as_ref().unwrap_or(&self.http));
        }
        match crate::network::network_type(host) {
            crate::network::Network::Tor => self.onion.as_ref(),
            crate::network::Network::I2p => self.i2p.as_ref(),
            crate::network::Network::Default => None,
        }
    }

    /// The unproxied client, for operator-pointed infrastructure (webhooks,
    /// the translation backend) that deliberately may live on internal
    /// addresses a proxy cannot dial.
    fn http_unproxied(&self) -> &reqwest::Client {
        self.direct.as_ref().unwrap_or(&self.http)
    }

    #[must_use]
    pub fn with_fetch_signer(mut self, signer: RequestSigner) -> Self {
        self.fetch_signer = Some(signer);
        self
    }

    #[must_use]
    pub fn with_page_user_agent(mut self, user_agent: String) -> Self {
        self.page_user_agent = Some(user_agent);
        self
    }

    /// Parses and SSRF-checks a URL before any request is made with it.
    fn checked(&self, raw_url: &str) -> Result<Url, FederationError> {
        let url: Url = raw_url
            .parse()
            .map_err(|_| FederationError::InvalidUrl(raw_url.to_owned()))?;
        guard::check_url_syntax(&url, self.allow_private)?;
        Ok(url)
    }

    /// Performs an `ActivityPub` GET of `url`, signed with the request-scoped
    /// signer when supplied and otherwise with the configured fetch signer,
    /// and decodes the JSON body. Like Mastodon's
    /// `valid_activitypub_content_type?`, the response must declare an
    /// `ActivityPub` content type (`activity+json`, or `ld+json` carrying the
    /// `ActivityStreams` profile) — anything a server happens to serve as
    /// plain JSON (user uploads, unrelated API endpoints) must not be
    /// dereferenceable as an `ActivityPub` object. A request-scoped signer is
    /// used when visibility is tied to one local recipient (followers-only
    /// and direct posts).
    async fn ap_get_as(
        &self,
        url: Url,
        signer: Option<&RequestSigner>,
    ) -> Result<Value, FederationError> {
        let accept = format!("{ACTIVITY_JSON}, {LD_JSON_AS}");
        match self
            .ap_get_with_accept_as(url.clone(), &accept, signer)
            .await
        {
            Ok(object) => Ok(object),
            // Funkwhale's actor endpoint has the same renderer negotiation
            // bug as its Audio endpoint. Retry only a structurally invalid AP
            // response; status, transport, SSRF and rate-limit failures keep
            // their original meaning and are never double-hit.
            Err(FederationError::InvalidActor(_)) => {
                self.ap_get_with_accept_as(url, ACTIVITY_JSON, signer).await
            }
            Err(error) => Err(error),
        }
    }

    /// The common AP GET machinery with a caller-selected negotiation string.
    /// Resource discovery normally advertises every usable representation,
    /// but a few real servers (notably Funkwhale 2.0) mis-negotiate that list
    /// while answering a single `application/activity+json` offer correctly.
    async fn ap_get_with_accept_as(
        &self,
        url: Url,
        accept: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<Value, FederationError> {
        let mut current = url;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self.send_signed_get_as(&current, accept, signer).await?;
            if response.status().is_redirection() {
                current = self.redirect_target(&current, &response)?;
                continue;
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(FederationError::RateLimited {
                    retry_after_secs: retry_after_secs(response.headers()),
                });
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let content_type = content_type(response.headers());
            if !is_activitypub_content_type(&content_type) {
                return Err(FederationError::InvalidActor(format!(
                    "{current} answered with non-ActivityPub content type {content_type:?}"
                )));
            }
            let body = limited_bytes(response, FEDERATION_JSON_LIMIT, &current).await?;
            return serde_json::from_slice(&body)
                .map_err(|e| FederationError::InvalidActor(e.to_string()));
        }
        Err(FederationError::InvalidUrl(format!(
            "{current}: too many redirects"
        )))
    }

    fn redirect_target(
        &self,
        current: &Url,
        response: &reqwest::Response,
    ) -> Result<Url, FederationError> {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
        let next = current
            .join(location)
            .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
        self.checked(next.as_str())
    }

    fn signed_get(
        &self,
        url: &Url,
        accept: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<reqwest::RequestBuilder, FederationError> {
        let mut request = self.http_for(url).get(url.clone()).header(ACCEPT, accept);
        if let Some(signer) = signer.or(self.fetch_signer.as_ref()) {
            let host =
                host_header(url).ok_or_else(|| FederationError::InvalidUrl(url.to_string()))?;
            let path_and_query = path_and_query(url);
            let headers = signer.sign_get(&host, &path_and_query, accept, SystemTime::now());
            request = request
                .header("Date", headers.date)
                .header("Signature", headers.signature);
        }
        Ok(request)
    }

    /// Performs a signed GET, double-knocking on refusal: a peer that answers
    /// `400`/`401` may simply be unable to resolve the draft-cavage `keyId`,
    /// so the same request is retried RFC 9421-signed with our Ed25519
    /// verification method.
    ///
    /// Mastodon 4.7 builds a remote actor's keys from `assertionMethod`, and
    /// builds in the 2026-06-19…07-06 window (mastodon#39725) let that
    /// replace `publicKey`, leaving the Ed25519 key as the *only* one they
    /// hold for us. We publish the RSA key as a Multikey too so the first
    /// knock normally lands; this covers the peers that already cached the
    /// stripped actor, and any future peer that keeps FEP-521a keys alone.
    async fn send_signed_get_as(
        &self,
        url: &Url,
        accept: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<reqwest::Response, FederationError> {
        self.send_signed_get_conditional(url, accept, None, signer)
            .await
    }

    async fn send_signed_get_conditional(
        &self,
        url: &Url,
        accept: &str,
        etag: Option<&str>,
        signer: Option<&RequestSigner>,
    ) -> Result<reqwest::Response, FederationError> {
        let signer = signer.or(self.fetch_signer.as_ref());
        let mut request = self.signed_get(url, accept, signer)?;
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        if !matches!(response.status().as_u16(), 400 | 401) {
            return Ok(response);
        }
        let Some(headers) = signer
            .and_then(|signer| signer.sign_get_rfc9421_ed25519(url.as_str(), SystemTime::now()))
        else {
            return Ok(response);
        };
        tracing::debug!(
            status = response.status().as_u16(),
            url = url.as_str(),
            "signed fetch refused; double-knocking with RFC 9421 (Ed25519)"
        );
        let mut retry = self
            .http_for(url)
            .get(url.clone())
            .header(ACCEPT, accept)
            .header("Date", headers.date)
            .header("Signature-Input", headers.signature_input)
            .header("Signature", headers.signature);
        if let Some(etag) = etag {
            retry = retry.header(IF_NONE_MATCH, etag);
        }
        let retried = retry.send().await?;
        // A retry that fares no better must not mask the original answer.
        if retried.status().is_success() || retried.status().is_redirection() {
            Ok(retried)
        } else {
            Ok(response)
        }
    }

    /// Builds one redirect-disabled media request. Media is public often
    /// enough that the first request stays anonymous; when an origin refuses
    /// it, [`Self::send_media_get`] rebuilds the exact request with the
    /// instance actor's draft-cavage signature. A redirect therefore starts a
    /// fresh anonymous hop; it only receives a signature if that hop also
    /// refuses anonymous access.
    fn media_get_request(
        &self,
        url: &Url,
        range: Option<&str>,
        timeout: Option<Duration>,
        signed: bool,
    ) -> Result<reqwest::RequestBuilder, FederationError> {
        let mut request = self
            .http_no_redirect_for(url)
            .get(url.clone())
            .header(ACCEPT, MEDIA_ACCEPT);
        if let Some(range) = range {
            request = request.header(reqwest::header::RANGE, range);
        }
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if signed && let Some(signer) = &self.fetch_signer {
            let host =
                host_header(url).ok_or_else(|| FederationError::InvalidUrl(url.to_string()))?;
            let signed =
                signer.sign_get(&host, &path_and_query(url), MEDIA_ACCEPT, SystemTime::now());
            request = request
                .header("Date", signed.date)
                .header("Signature", signed.signature);
        }
        Ok(request)
    }

    /// Fetches one media response, retrying a 401/403 once with the instance
    /// actor signature used for authorized `ActivityPub` fetches. Funkwhale's
    /// default-private listen endpoint grants such actors `read:libraries`.
    async fn send_media_get(
        &self,
        url: &Url,
        range: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<reqwest::Response, FederationError> {
        let response = self
            .media_get_request(url, range, timeout, false)?
            .send()
            .await?;
        if !matches!(response.status().as_u16(), 401 | 403) || self.fetch_signer.is_none() {
            return Ok(response);
        }
        tracing::debug!(
            status = response.status().as_u16(),
            url = url.as_str(),
            "anonymous media fetch refused; retrying with instance signature"
        );
        Ok(self
            .media_get_request(url, range, timeout, true)?
            .send()
            .await?)
    }

    async fn fetch_resource_hop(
        &self,
        uri: &str,
        terminal: bool,
        signer: Option<&RequestSigner>,
    ) -> Result<ResourceHop, FederationError> {
        let mut url = self.checked(uri)?;
        let mut final_response = None;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                .send_signed_get_as(&url, RESOURCE_ACCEPT, signer)
                .await?;
            if !response.status().is_redirection() {
                final_response = Some(response);
                break;
            }
            url = self.redirect_target(&url, &response)?;
        }
        let response = final_response
            .ok_or_else(|| FederationError::InvalidUrl(format!("{uri}: too many redirects")))?;
        if !response.status().is_success() {
            return Err(FederationError::Status(response.status().as_u16()));
        }

        let final_url = response.url().clone();
        let headers = response.headers().clone();
        let content_type = content_type(&headers);

        if is_activitypub_content_type(&content_type) {
            let body = limited_bytes(response, FEDERATION_JSON_LIMIT, &final_url).await?;
            let object = serde_json::from_slice(&body)
                .map_err(|e| FederationError::InvalidActor(e.to_string()))?;
            return Ok(ResourceHop::Object(object));
        }

        if !terminal
            && let Some(alternate) = activitypub_alternate_from_link_headers(&headers, &final_url)
        {
            return Ok(ResourceHop::Alternate(alternate));
        }

        // Mastodon's `FetchResourceService` parity: a response that is not an
        // ActivityPub content type is only ever a stepping stone to an
        // advertised AP alternate, never parsed as the object itself.
        if !terminal && is_html(&content_type) {
            let body = limited_text(response).await?;
            if let Some(alternate) = activitypub_alternate_from_html(&body, &final_url) {
                return Ok(ResourceHop::Alternate(alternate));
            }
        }

        // Funkwhale 2.0's DRF renderer does not negotiate Mastodon's resource
        // Accept list correctly: it redirects when `ld+json` leads, and emits
        // an unprofiled `application/ld+json` when `activity+json` merely leads.
        // An exact `application/activity+json` request returns the same Audio
        // object with the correct content type. Keep the ordinary HTML/
        // alternate discovery first, then make that narrow compatibility
        // retry without ever relaxing response content-type validation.
        if !terminal
            && let Ok(object) = self
                .ap_get_with_accept_as(self.checked(uri)?, ACTIVITY_JSON, signer)
                .await
        {
            return Ok(ResourceHop::Object(object));
        }

        Err(FederationError::InvalidActor(format!(
            "{uri} did not return an ActivityPub object"
        )))
    }

    /// Dereferences an actor URI. The returned document's `id` is required to
    /// match the requested URI — a server must not be able to impersonate an
    /// actor hosted elsewhere.
    pub async fn fetch_actor(&self, uri: &str) -> Result<RemoteActor, FederationError> {
        self.fetch_actor_as(uri, None).await
    }

    /// Dereferences an actor with a request-scoped local signer.
    pub async fn fetch_actor_as(
        &self,
        uri: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<RemoteActor, FederationError> {
        let url = self.checked(uri)?;
        let document = self.ap_get_as(url, signer).await?;
        let actor: RemoteActor = serde_json::from_value(document)
            .map_err(|e| FederationError::InvalidActor(e.to_string()))?;
        if actor.id != uri {
            return Err(FederationError::InvalidActor(format!(
                "actor id {} does not match fetched uri {uri}",
                actor.id
            )));
        }
        if actor.preferred_username.is_empty() && actor.webfinger_acct().is_none() {
            return Err(FederationError::InvalidActor(format!(
                "{uri} has neither preferredUsername nor webfinger"
            )));
        }
        Ok(actor)
    }

    /// Fetches an arbitrary `ActivityPub` object (e.g. a `QuoteAuthorization`
    /// stamp). The returned object's `id` must match the requested URI.
    pub async fn fetch_object(&self, uri: &str) -> Result<Value, FederationError> {
        self.fetch_object_as(uri, None).await
    }

    /// Fetches an object with a request-scoped local signer.
    pub async fn fetch_object_as(
        &self,
        uri: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<Value, FederationError> {
        let url = self.checked(uri)?;
        let object = self.ap_get_as(url, signer).await?;
        if object.get("id").and_then(Value::as_str) != Some(uri) {
            return Err(FederationError::InvalidActor(format!(
                "object id does not match fetched uri {uri}"
            )));
        }
        Ok(object)
    }

    /// Conditional variant used by bounded outbox hydration. It keeps all of
    /// the ordinary federation protections: signed GETs, per-hop SSRF checks,
    /// redirect cap, `ActivityPub` content-type validation, and the 2 MiB body
    /// ceiling. A collection id may name either the requested URL or the final
    /// redirect URL; any other id is rejected.
    pub async fn fetch_activitypub(
        &self,
        uri: &str,
        etag: Option<&str>,
    ) -> Result<FetchedActivityPub, FederationError> {
        self.fetch_activitypub_with_redirect_guard(uri, etag, |_| async { Ok(true) })
            .await
    }

    /// Conditional AP fetch with a caller-owned policy check before every
    /// redirect hop. The transport still performs its own syntax/SSRF checks;
    /// this callback lets the server apply mutable federation-domain policy
    /// before the redirected origin receives a request.
    pub async fn fetch_activitypub_with_redirect_guard<F, Fut>(
        &self,
        uri: &str,
        etag: Option<&str>,
        mut allow_redirect: F,
    ) -> Result<FetchedActivityPub, FederationError>
    where
        F: FnMut(&str) -> Fut,
        Fut: std::future::Future<Output = Result<bool, FederationError>>,
    {
        let accept = format!("{ACTIVITY_JSON}, {LD_JSON_AS}");
        let requested = self.checked(uri)?;
        let mut current = requested.clone();
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                .send_signed_get_conditional(&current, &accept, etag, None)
                .await?;
            if response.status().is_redirection() {
                let next = self.redirect_target(&current, &response)?;
                if !allow_redirect(next.as_str()).await? {
                    return Err(FederationError::InvalidUrl(format!(
                        "redirect to {next} is blocked by instance policy"
                    )));
                }
                current = next;
                continue;
            }
            if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                return Ok(FetchedActivityPub {
                    document: None,
                    etag: response
                        .headers()
                        .get(ETAG)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                    final_url: current.to_string(),
                    body_bytes: 0,
                });
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(FederationError::RateLimited {
                    retry_after_secs: retry_after_secs(response.headers()),
                });
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let content_type = content_type(response.headers());
            if !is_activitypub_content_type(&content_type) {
                return Err(FederationError::InvalidActor(format!(
                    "{current} answered with non-ActivityPub content type {content_type:?}"
                )));
            }
            let response_etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = limited_bytes(response, FEDERATION_JSON_LIMIT, &current).await?;
            let document: Value = serde_json::from_slice(&body)
                .map_err(|error| FederationError::InvalidActor(error.to_string()))?;
            if let Some(id) = document.get("id").and_then(Value::as_str)
                && id != requested.as_str()
                && id != current.as_str()
            {
                return Err(FederationError::InvalidActor(format!(
                    "collection id {id} does not match requested or redirected uri"
                )));
            }
            return Ok(FetchedActivityPub {
                document: Some(document),
                etag: response_etag,
                final_url: current.to_string(),
                body_bytes: body.len(),
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{uri}: too many redirects"
        )))
    }

    /// Resolves a user-supplied URL that may be a *permalink* (an object's
    /// `url`, e.g. Mastodon's `/@user/123`) rather than its canonical `id`.
    /// The document is fetched with Mastodon's `FetchResourceService`
    /// negotiation: `ActivityPub` JSON first, with HTML accepted only so an
    /// advertised AP alternate (`Link` header or `<link rel="alternate">`) can
    /// be followed. When a JSON body's `id` differs from the fetched URL, the
    /// canonical `id` is re-dereferenced once and id-validated. The returned
    /// object's `id` is the canonical one.
    pub async fn fetch_object_following(&self, uri: &str) -> Result<Value, FederationError> {
        self.fetch_object_following_as(uri, None).await
    }

    /// Resolves a permalink/canonical object pair with one local actor's
    /// signer on every hop.
    pub async fn fetch_object_following_as(
        &self,
        uri: &str,
        signer: Option<&RequestSigner>,
    ) -> Result<Value, FederationError> {
        let mut current = uri.to_owned();
        let mut terminal = false;
        loop {
            match self.fetch_resource_hop(&current, terminal, signer).await? {
                ResourceHop::Alternate(alternate) => {
                    if terminal {
                        return Err(FederationError::InvalidActor(format!(
                            "{current} returned a nested ActivityPub alternate"
                        )));
                    }
                    current = alternate;
                    terminal = true;
                }
                ResourceHop::Object(object) => match object.get("id").and_then(Value::as_str) {
                    Some(id) if id == current => return Ok(object),
                    Some(id) if !terminal => {
                        id.clone_into(&mut current);
                        terminal = true;
                    }
                    Some(_) => {
                        return Err(FederationError::InvalidActor(format!(
                            "object id does not match fetched uri {current}"
                        )));
                    }
                    None => {
                        return Err(FederationError::InvalidActor(format!(
                            "object at {current} has no id"
                        )));
                    }
                },
            }
        }
    }

    /// Fetches a web page (or oEmbed document) for link previews. The URL is
    /// user-supplied, so every redirect hop passes the SSRF guard before it
    /// is followed; the body is truncated at [`PAGE_BODY_LIMIT`] and decoded
    /// as UTF-8 (lossily). Returns the final URL of the redirect chain.
    pub async fn fetch_page(
        &self,
        raw_url: &str,
        accept: &str,
    ) -> Result<FetchedPage, FederationError> {
        let mut url = self.checked(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let mut request = self
                .http_no_redirect_for(&url)
                .get(url.clone())
                .header(ACCEPT, accept)
                .header(ACCEPT_LANGUAGE, PAGE_ACCEPT_LANGUAGE);
            if let Some(agent) = &self.page_user_agent {
                request = request.header(USER_AGENT, agent);
            }
            let response = request.send().await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                let next = url
                    .join(location)
                    .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut body = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                // Truncate rather than fail: metadata lives at the top.
                let room = PAGE_BODY_LIMIT - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if body.len() >= PAGE_BODY_LIMIT {
                    break;
                }
            }
            return Ok(FetchedPage {
                final_url: url.to_string(),
                content_type,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Downloads a remote media file (an attachment, avatar or header) for
    /// local caching. Like [`Self::fetch_page`] the URL is remote-controlled,
    /// so every redirect hop passes the SSRF guard before it is followed; the
    /// body is capped at [`MEDIA_BODY_LIMIT`] and returned as raw bytes (no
    /// UTF-8 decode). Returns the final URL of the redirect chain.
    pub async fn fetch_media(&self, raw_url: &str) -> Result<FetchedMedia, FederationError> {
        let mut url = self.checked(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self.send_media_get(&url, None, None).await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                let next = url
                    .join(location)
                    .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            // Reject up front when the advertised length already exceeds the cap.
            if response
                .content_length()
                .is_some_and(|len| len > MEDIA_BODY_LIMIT as u64)
            {
                return Err(FederationError::TooLarge(url.to_string()));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut bytes = Vec::new();
            let mut response = response;
            while let Some(chunk) = response.chunk().await? {
                if bytes.len() + chunk.len() > MEDIA_BODY_LIMIT {
                    return Err(FederationError::TooLarge(url.to_string()));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(FetchedMedia {
                final_url: url.to_string(),
                content_type,
                bytes,
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Downloads a large remote media file (remote video caching), streaming
    /// the body to `dest` instead of buffering it — memory use stays constant
    /// however big the file. Same SSRF discipline as [`Self::fetch_media`]
    /// (every redirect hop re-checked), but the size cap is the caller's
    /// `max_bytes` budget and the per-request timeout is replaced by a
    /// stall detector (a gigabyte download cannot finish in 15 s).
    pub async fn fetch_media_to_file(
        &self,
        raw_url: &str,
        dest: &std::path::Path,
        max_bytes: u64,
    ) -> Result<FetchedMediaFile, FederationError> {
        use tokio::io::AsyncWriteExt;

        let mut url = self.checked(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                .send_media_get(&url, None, Some(AV_FETCH_TIMEOUT))
                .await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                let next = url
                    .join(location)
                    .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            // Reject up front when the advertised length already exceeds the cap.
            if response.content_length().is_some_and(|len| len > max_bytes) {
                return Err(FederationError::TooLarge(url.to_string()));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let mut file = tokio::fs::File::create(dest).await?;
            let mut written: u64 = 0;
            let mut response = response;
            loop {
                let chunk = tokio::time::timeout(AV_CHUNK_TIMEOUT, response.chunk())
                    .await
                    .map_err(|_| FederationError::Stalled(url.to_string()))??;
                let Some(chunk) = chunk else { break };
                written += chunk.len() as u64;
                if written > max_bytes {
                    return Err(FederationError::TooLarge(url.to_string()));
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            return Ok(FetchedMediaFile {
                final_url: url.to_string(),
                content_type,
                bytes_written: written,
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Fetches a single byte range of a remote file — the caching HLS
    /// reverse-proxy's segment fetch. Sends `Range: bytes=start-(start+len-1)`
    /// and streams the response to `dest` (constant memory). Same SSRF
    /// discipline as the other fetchers (every redirect hop re-checked).
    /// `PeerTube` serves HLS as public, Range-capable static files, so the origin answers
    /// `206` and the body *is* the slice; a `200` (origin ignored the range)
    /// is handled by dropping the leading `start` bytes then keeping `len`.
    /// Returns the number of bytes written.
    pub async fn fetch_range_to_file(
        &self,
        raw_url: &str,
        dest: &std::path::Path,
        start: u64,
        len: u64,
    ) -> Result<u64, FederationError> {
        use tokio::io::AsyncWriteExt;

        let end = start.saturating_add(len).saturating_sub(1);
        let range = format!("bytes={start}-{end}");
        let mut url = self.checked(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                .send_media_get(&url, Some(&range), Some(SEGMENT_FETCH_TIMEOUT))
                .await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                let next = url
                    .join(location)
                    .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let honored = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
            let mut file = tokio::fs::File::create(dest).await?;
            let mut written: u64 = 0;
            let mut skipped: u64 = 0;
            let mut response = response;
            while written < len {
                let chunk = tokio::time::timeout(AV_CHUNK_TIMEOUT, response.chunk())
                    .await
                    .map_err(|_| FederationError::Stalled(url.to_string()))??;
                let Some(mut chunk) = chunk else { break };
                if !honored && skipped < start {
                    // Origin ignored the range: drop the leading `start` bytes.
                    let drop = usize::try_from((start - skipped).min(chunk.len() as u64))
                        .unwrap_or(chunk.len());
                    skipped += drop as u64;
                    chunk = chunk.split_off(drop);
                    if chunk.is_empty() {
                        continue;
                    }
                }
                let take =
                    usize::try_from((len - written).min(chunk.len() as u64)).unwrap_or(chunk.len());
                file.write_all(&chunk[..take]).await?;
                written += take as u64;
            }
            file.flush().await?;
            return Ok(written);
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Opens one exact byte range of public remote media as a validated stream.
    /// Unlike [`Self::fetch_range_to_file`], a non-zero range MUST receive a
    /// correct `206 Content-Range`; accepting `200` and discarding a potentially
    /// multi-gigabyte prefix would let a range-ignoring origin amplify a seek
    /// into a whole-file download.
    pub async fn fetch_media_range(
        &self,
        raw_url: &str,
        start: u64,
        len: u64,
    ) -> Result<FetchedMediaRange, FederationError> {
        if len == 0 {
            return Err(FederationError::InvalidUrl(format!(
                "{raw_url}: empty media range"
            )));
        }
        let end = start
            .checked_add(len - 1)
            .ok_or_else(|| FederationError::InvalidUrl(format!("{raw_url}: range overflow")))?;
        let range = format!("bytes={start}-{end}");
        let mut url = self.checked(raw_url)?;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                // The client-wide timeout bounds time to response headers.
                // Per-chunk stalls are bounded by the stream below without
                // imposing a whole-body timeout on a slow but active viewer.
                .send_media_get(&url, Some(&range), None)
                .await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FederationError::Status(response.status().as_u16()))?;
                let next = url
                    .join(location)
                    .map_err(|_| FederationError::InvalidUrl(location.to_owned()))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(FederationError::Status(response.status().as_u16()));
            }
            let content_range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range)
                .ok_or_else(|| {
                    FederationError::InvalidUrl(format!("{url}: missing or invalid Content-Range"))
                })?;
            if content_range.0 != start || content_range.1 != end {
                return Err(FederationError::InvalidUrl(format!(
                    "{url}: origin returned bytes {}-{}/{} for requested {start}-{end}",
                    content_range.0, content_range.1, content_range.2
                )));
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let final_url = url.to_string();
            let bytes = stream::try_unfold(response, |mut response| async move {
                let chunk = tokio::time::timeout(AV_CHUNK_TIMEOUT, response.chunk())
                    .await
                    .map_err(|_| FederationError::Stalled(response.url().to_string()))??;
                Ok(chunk.map(|bytes| (bytes.to_vec(), response)))
            });
            return Ok(FetchedMediaRange {
                final_url,
                content_type,
                total_len: content_range.2,
                start,
                len,
                bytes: Box::pin(bytes),
            });
        }
        Err(FederationError::InvalidUrl(format!(
            "{raw_url}: too many redirects"
        )))
    }

    /// Resolves `user@domain` to an actor URI via `WebFinger`.
    pub async fn resolve_acct(&self, acct: &Acct) -> Result<ResolvedAcct, FederationError> {
        let resolved = self.resolve_acct_once(acct).await?;
        if same_acct(&resolved.acct, acct) {
            return Ok(resolved);
        }

        // Like Mastodon, allow one WebFinger subject redirect, but require the
        // second lookup to confirm the redirected subject.
        let redirected = self.resolve_acct_once(&resolved.acct).await?;
        if same_acct(&redirected.acct, &resolved.acct) {
            Ok(redirected)
        } else {
            Err(FederationError::InvalidActor(format!(
                "too many webfinger redirects for {acct}"
            )))
        }
    }

    async fn resolve_acct_once(&self, acct: &Acct) -> Result<ResolvedAcct, FederationError> {
        // Cap the candidates parsed from one JRD defensively — a JRD advertising
        // a crowd of actors for one handle is hostile, not a real Person+Group
        // pair.
        const MAX_CANDIDATES: usize = 8;
        let url = self.checked(&webfinger_url(acct))?;
        let mut current = url;
        let mut final_response = None;
        for _ in 0..=PAGE_MAX_REDIRECTS {
            let response = self
                .http_for(&current)
                .get(current.clone())
                .header(ACCEPT, "application/jrd+json, application/json")
                .send()
                .await?;
            if !response.status().is_redirection() {
                final_response = Some(response);
                break;
            }
            current = self.redirect_target(&current, &response)?;
        }
        let response = final_response
            .ok_or_else(|| FederationError::InvalidUrl(format!("{acct}: too many redirects")))?;
        if !response.status().is_success() {
            return Err(FederationError::Status(response.status().as_u16()));
        }
        let body = limited_bytes(response, FEDERATION_JSON_LIMIT, &current).await?;
        let jrd: Jrd = serde_json::from_slice(&body)
            .map_err(|e| FederationError::InvalidActor(e.to_string()))?;
        let subject: Acct = jrd
            .subject
            .parse()
            .map_err(|e: plamenu_ap::acct::AcctError| {
                FederationError::InvalidActor(e.to_string())
            })?;
        // Collect every AP-usable `self` link, in JRD order, deduplicating by
        // actor URI (capped by `MAX_CANDIDATES`), plus the remote-interaction
        // subscribe template when advertised.
        let mut candidates: Vec<WebfingerCandidate> = Vec::new();
        let mut subscribe_template = None;
        let mut hls_stream_url = None;
        for link in jrd.links {
            if link.rel == "http://ostatus.org/schema/1.0/subscribe" {
                if subscribe_template.is_none() {
                    subscribe_template.clone_from(&link.template);
                }
                continue;
            }
            if link.rel == "alternate"
                && link.media_type.as_deref().is_some_and(|media_type| {
                    media_type.eq_ignore_ascii_case("application/x-mpegURL")
                        || media_type.eq_ignore_ascii_case("application/vnd.apple.mpegurl")
                })
            {
                if hls_stream_url.is_none() {
                    hls_stream_url = link.href;
                }
                continue;
            }
            if link.rel != "self"
                || !link
                    .media_type
                    .as_deref()
                    .is_some_and(|t| t == ACTIVITY_JSON || t.starts_with("application/ld+json"))
            {
                continue;
            }
            let advertised_type = link.advertised_type().map(str::to_owned);
            let Some(href) = link.href else {
                continue;
            };
            if candidates.iter().any(|c| c.actor_uri == href) {
                continue;
            }
            if candidates.len() >= MAX_CANDIDATES {
                continue;
            }
            candidates.push(WebfingerCandidate {
                actor_uri: href,
                advertised_type,
            });
        }
        if candidates.is_empty() {
            return Err(FederationError::NoSelfLink(acct.to_string()));
        }
        // Primary = the first person-like candidate, else the first. Preserves
        // the Mastodon-compatible default: a bare `@name@host` resolves to the
        // user when a host also serves a same-named group; a group-only handle
        // resolves to the group.
        let actor_uri = candidates
            .iter()
            .find(|c| !c.is_group_hint())
            .unwrap_or(&candidates[0])
            .actor_uri
            .clone();
        Ok(ResolvedAcct {
            acct: subject,
            actor_uri,
            candidates,
            subscribe_template,
            hls_stream_url,
        })
    }

    /// Posts an encrypted Web Push message (RFC 8030) to a push service
    /// endpoint, returning the HTTP status — the caller decides which codes
    /// kill the subscription. Endpoints are client-supplied URLs, so the
    /// SSRF guard applies like everywhere else.
    pub async fn web_push(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<u16, FederationError> {
        let url = self.checked(endpoint)?;
        let mut request = self
            .http_for(&url)
            .post(url.clone())
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        Ok(response.status().as_u16())
    }

    /// Posts a JSON webhook payload to an admin-configured callback URL,
    /// returning the HTTP status. No SSRF guard: webhook URLs are set by
    /// admins only and deliberately may point at internal services
    /// (Mastodon's delivery worker uses `allow_local: true`) — which is also
    /// why this bypasses any configured proxy: a SOCKS proxy cannot dial the
    /// operator's internal addresses.
    pub async fn webhook_post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: String,
    ) -> Result<u16, FederationError> {
        let url: Url = url
            .parse()
            .map_err(|_| FederationError::InvalidUrl(url.to_owned()))?;
        let mut request = self
            .http_unproxied()
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        Ok(response.status().as_u16())
    }

    /// Performs a plain GET/POST to an operator-configured service (the
    /// translation backend), returning the HTTP status and the response body
    /// (truncated at [`PAGE_BODY_LIMIT`], decoded lossily as UTF-8). Like
    /// [`Self::webhook_post`] there is no SSRF guard: the endpoint is set by
    /// the operator (a `LibreTranslate` endpoint may point at a loopback
    /// service, Mastodon's `allow_local: true`), and `DeepL` is a fixed host.
    /// The caller inspects the status to distinguish rate-limit/quota errors.
    pub async fn service_request(
        &self,
        method: HttpMethod,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<ServiceResponse, FederationError> {
        let url: Url = url
            .parse()
            .map_err(|_| FederationError::InvalidUrl(url.to_owned()))?;
        // Operator-pointed infrastructure like the webhook path: a
        // LibreTranslate/llama-server endpoint may live on a loopback or LAN
        // address no proxy can dial, so this bypasses any configured proxy.
        let mut request = match method {
            HttpMethod::Get => self.http_unproxied().get(url),
            HttpMethod::Post => self.http_unproxied().post(url),
        };
        // A self-hosted CPU model backend legitimately takes far longer than
        // the client-wide 15s budget; the caller may widen this one request.
        if let Some(secs) = timeout_secs {
            request = request.timeout(std::time::Duration::from_secs(secs));
        }
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let mut response = response;
        let mut buf = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            let room = PAGE_BODY_LIMIT - buf.len();
            buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
            if buf.len() >= PAGE_BODY_LIMIT {
                break;
            }
        }
        Ok(ServiceResponse {
            status,
            body: String::from_utf8_lossy(&buf).into_owned(),
        })
    }

    /// Delivers an activity to an inbox with a signed POST. With
    /// `try_rfc9421`, the delivery double-knocks: RFC 9421 first, and when
    /// the remote refuses the request outright (400/401/403 — how servers
    /// that only read the draft-cavage `Signature` header answer what looks
    /// unsigned to them), the same delivery is retried draft-cavage-signed.
    /// Returns the style that succeeded so the caller can remember the
    /// host's answer.
    pub async fn deliver(
        &self,
        activity: &Value,
        inbox_url: &str,
        signer: &RequestSigner,
        headers: &[(String, String)],
        try_rfc9421: bool,
    ) -> Result<SignatureStyle, FederationError> {
        double_knock(try_rfc9421, |style| {
            self.deliver_styled(activity, inbox_url, signer, headers, style)
        })
        .await
    }

    async fn deliver_styled(
        &self,
        activity: &Value,
        inbox_url: &str,
        signer: &RequestSigner,
        headers: &[(String, String)],
        style: SignatureStyle,
    ) -> Result<(), FederationError> {
        self.deliver_with(
            activity,
            inbox_url,
            signer,
            style,
            |url, signed_headers, body| async move {
                let mut request = self
                    .http_no_redirect_for(&url)
                    .post(url)
                    .header(CONTENT_TYPE, ACTIVITY_JSON);
                for (name, value) in &signed_headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                for (name, value) in headers {
                    request = request.header(name, value);
                }
                let response = request.body(body).send().await?;
                Ok(DeliveryResponse {
                    status: response.status(),
                    location: response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    retry_after_secs: response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse().ok()),
                })
            },
        )
        .await
    }

    async fn deliver_with<Post, Fut>(
        &self,
        activity: &Value,
        inbox_url: &str,
        signer: &RequestSigner,
        style: SignatureStyle,
        mut post: Post,
    ) -> Result<(), FederationError>
    where
        Post: FnMut(Url, Vec<(String, String)>, Vec<u8>) -> Fut,
        Fut: Future<Output = Result<DeliveryResponse, FederationError>>,
    {
        let mut url = self.checked(inbox_url)?;
        let body = serde_json::to_vec(activity).expect("activity json is serializable");

        for _ in 0..=DELIVERY_MAX_REDIRECTS {
            let host =
                host_header(&url).ok_or_else(|| FederationError::InvalidUrl(url.to_string()))?;
            let path_and_query = path_and_query(&url);
            let signed_headers = match style {
                SignatureStyle::Cavage => {
                    let produced =
                        signer.sign_post(&host, &path_and_query, &body, SystemTime::now());
                    vec![
                        ("Date".to_owned(), produced.date),
                        ("Digest".to_owned(), produced.digest),
                        ("Signature".to_owned(), produced.signature),
                    ]
                }
                SignatureStyle::Rfc9421 => {
                    let produced = signer.sign_post_rfc9421(url.as_str(), &body, SystemTime::now());
                    vec![
                        ("Date".to_owned(), produced.date),
                        ("Content-Digest".to_owned(), produced.content_digest),
                        ("Signature-Input".to_owned(), produced.signature_input),
                        ("Signature".to_owned(), produced.signature),
                    ]
                }
            };

            let response = post(url.clone(), signed_headers, body.clone()).await?;
            if response.status.is_redirection() {
                let location = response
                    .location
                    .ok_or_else(|| FederationError::Status(response.status.as_u16()))?;
                let next = url
                    .join(&location)
                    .map_err(|_| FederationError::InvalidUrl(location))?;
                url = self.checked(next.as_str())?;
                continue;
            }
            if response.status.as_u16() == 429 {
                return Err(FederationError::RateLimited {
                    retry_after_secs: response.retry_after_secs,
                });
            }
            if !response.status.is_success() {
                return Err(FederationError::Status(response.status.as_u16()));
            }
            return Ok(());
        }
        Err(FederationError::InvalidUrl(format!(
            "{inbox_url}: too many redirects"
        )))
    }
}

/// One outbound HTTP client: redirects off (hops are walked manually so the
/// SSRF guard sees every target), [`guard::SafeResolver`] installed, and an
/// optional proxy for the overlay-network lanes.
fn build_client(
    user_agent: &str,
    allow_private: bool,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<reqwest::Client, FederationError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(guard::SafeResolver::shared(allow_private));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| {
            FederationError::InvalidUrl("configured proxy URL is invalid".to_owned())
        })?);
    }
    Ok(builder.build()?)
}

fn parse_content_range(raw: &str) -> Option<(u64, u64, u64)> {
    let raw = raw.strip_prefix("bytes ")?;
    let (range, total) = raw.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (start <= end && end < total).then_some((start, end, total))
}

/// The double-knock policy, separated from the transport so it is testable:
/// RFC 9421 first when allowed; after any HTTP-status answer (401/403 is how
/// draft-cavage-only servers answer what looks unsigned to them, but header
/// parsers in the wild also 400 or 500 on the unfamiliar `sig1=:…:` shape)
/// the same delivery is retried draft-cavage-signed. Rate limiting and
/// transport errors propagate: they say nothing about signature support and
/// affect both dialects alike.
async fn double_knock<Fut>(
    try_rfc9421: bool,
    mut attempt: impl FnMut(SignatureStyle) -> Fut,
) -> Result<SignatureStyle, FederationError>
where
    Fut: Future<Output = Result<(), FederationError>>,
{
    if try_rfc9421 {
        match attempt(SignatureStyle::Rfc9421).await {
            Ok(()) => return Ok(SignatureStyle::Rfc9421),
            Err(FederationError::Status(status)) => {
                tracing::debug!(
                    status,
                    "RFC 9421 delivery refused; double-knocking with draft-cavage"
                );
            }
            Err(other) => return Err(other),
        }
    }
    attempt(SignatureStyle::Cavage).await?;
    Ok(SignatureStyle::Cavage)
}

async fn limited_text(mut response: reqwest::Response) -> Result<String, FederationError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = PAGE_BODY_LIMIT - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= PAGE_BODY_LIMIT {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn limited_bytes(
    mut response: reqwest::Response,
    limit: usize,
    url: &Url,
) -> Result<Vec<u8>, FederationError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(FederationError::DocumentTooLarge(url.to_string()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if document_size_after(body.len(), chunk.len(), limit).is_none() {
            return Err(FederationError::DocumentTooLarge(url.to_string()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn document_size_after(current: usize, chunk: usize, limit: usize) -> Option<usize> {
    current.checked_add(chunk).filter(|size| *size <= limit)
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn mime_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or("").trim()
}

fn is_activitypub_content_type(content_type: &str) -> bool {
    let mime = mime_type(content_type);
    if mime == ACTIVITY_JSON {
        return true;
    }
    if mime != "application/ld+json" {
        return false;
    }
    content_type.split(';').skip(1).any(|param| {
        let Some((name, value)) = param.split_once('=') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("profile")
            && unquote(value).split_ascii_whitespace().any(|profile| {
                profile.eq_ignore_ascii_case("https://www.w3.org/ns/activitystreams")
            })
    })
}

fn is_html(content_type: &str) -> bool {
    mime_type(content_type) == "text/html"
}

fn activitypub_alternate_from_link_headers(headers: &HeaderMap, base: &Url) -> Option<String> {
    headers
        .get_all(LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| activitypub_alternate_from_link_header(value, base))
}

fn activitypub_alternate_from_link_header(header: &str, base: &Url) -> Option<String> {
    split_quoted(header, ',')
        .into_iter()
        .find_map(|part| activitypub_alternate_from_link_value(part, base))
}

fn activitypub_alternate_from_link_value(value: &str, base: &Url) -> Option<String> {
    let value = value.trim();
    let rest = value.strip_prefix('<')?;
    let (href, params) = rest.split_once('>')?;
    let mut rel = String::new();
    let mut kind = String::new();
    for param in split_quoted(params, ';') {
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "rel" => rel = unquote(value),
            "type" => kind = unquote(value),
            _ => {}
        }
    }
    (rel_token_includes(&rel, "alternate") && is_activitypub_link_type(&kind))
        .then(|| resolve_href(base, href))
        .flatten()
}

fn activitypub_alternate_from_html(html: &str, base: &Url) -> Option<String> {
    let tokenizer = Tokenizer::new(AlternateSink::default(), TokenizerOpts::default());
    let input = html5ever::buffer_queue::BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer
        .sink
        .href
        .into_inner()
        .and_then(|href| resolve_href(base, &href))
}

#[derive(Default)]
struct AlternateSink {
    href: std::cell::RefCell<Option<String>>,
}

impl AlternateSink {
    fn process_start_tag(&self, tag: &Tag) -> TokenSinkResult<()> {
        match &*tag.name {
            "link" => {
                if self.href.borrow().is_none() {
                    let rel = attr(tag, "rel").unwrap_or("");
                    let kind = attr(tag, "type").unwrap_or("");
                    if rel_token_includes(rel, "alternate")
                        && is_activitypub_link_type(kind)
                        && let Some(href) = attr(tag, "href").filter(|href| !href.trim().is_empty())
                    {
                        *self.href.borrow_mut() = Some(href.to_owned());
                    }
                }
            }
            "textarea" | "title" => return TokenSinkResult::RawData(RawKind::Rcdata),
            "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                return TokenSinkResult::RawData(RawKind::Rawtext);
            }
            "plaintext" => return TokenSinkResult::Plaintext,
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

impl TokenSink for AlternateSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => self.process_start_tag(&tag),
            _ => TokenSinkResult::Continue,
        }
    }
}

fn attr<'t>(tag: &'t Tag, name: &str) -> Option<&'t str> {
    tag.attrs
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| &*a.value)
}

fn rel_token_includes(rel: &str, token: &str) -> bool {
    rel.split_ascii_whitespace()
        .any(|part| part.eq_ignore_ascii_case(token))
}

fn same_acct(left: &Acct, right: &Acct) -> bool {
    left.username().eq_ignore_ascii_case(right.username())
        && left.domain().eq_ignore_ascii_case(right.domain())
}

fn is_activitypub_link_type(kind: &str) -> bool {
    let normalized = kind.trim();
    let mime = mime_type(normalized);
    if mime.eq_ignore_ascii_case(ACTIVITY_JSON) {
        return true;
    }
    if !mime.eq_ignore_ascii_case("application/ld+json") {
        return false;
    }
    normalized.split(';').skip(1).any(|param| {
        let Some((name, value)) = param.split_once('=') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("profile")
            && unquote(value).split_ascii_whitespace().any(|profile| {
                profile.eq_ignore_ascii_case("https://www.w3.org/ns/activitystreams")
            })
    })
}

fn resolve_href(base: &Url, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    let resolved = base.join(href).ok()?;
    (matches!(resolved.scheme(), "http" | "https") && resolved.host_str().is_some())
        .then(|| resolved.to_string())
}

fn split_quoted(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quote = false;
    let mut escaped = false;
    let mut start = 0;
    for (idx, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            _ if ch == separator && !in_quote => {
                parts.push(value[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(value[start..].trim());
    parts
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_owned();
    };
    let mut result = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            result.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            result.push(ch);
        }
    }
    if escaped {
        result.push('\\');
    }
    result
}

/// The `WebFinger` lookup URL for an acct. The scheme comes from
/// [`crate::network::guess_protocol`]: hidden services serve plain HTTP, and
/// hardcoding `https` here is the Pleroma bug that breaks handle-based
/// discovery of onion accounts. The acct domain may carry a port (dev rigs),
/// which is not part of the protocol guess.
fn webfinger_url(acct: &Acct) -> String {
    let domain = acct.domain();
    let host = domain.rsplit_once(':').map_or(domain, |(host, _)| host);
    format!(
        "{}://{domain}/.well-known/webfinger?resource=acct:{acct}",
        crate::network::guess_protocol(host)
    )
}

/// The signed `(request-target)` path: the URL's path plus any query string.
fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

/// The `Host` header value: hostname, plus the port when non-default.
fn host_header(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use http::header::{HeaderName, HeaderValue};
    use plamenu_ap::keys::generate_keypair;
    use serde_json::json;

    use super::*;
    use crate::signature::{PreparedVerification, SignatureError};

    /// The exact strings on the wire are an interop contract: strict peers
    /// (Mitra among them) reject fetches whose Accept lacks the profiled
    /// `ld+json` form, and Mastodon rejects responses without an AP content
    /// type. Change these only with a failing-interop reason.
    #[test]
    fn outgoing_negotiation_strings_are_pinned() {
        assert_eq!(
            format!("{ACTIVITY_JSON}, {LD_JSON_AS}"),
            "application/activity+json, \
             application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        );
        assert_eq!(
            RESOURCE_ACCEPT,
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\", \
             application/activity+json, text/html;q=0.1"
        );
    }

    #[test]
    fn federation_document_limit_rejects_overflow_and_oversize_chunks() {
        assert_eq!(document_size_after(10, 5, 15), Some(15));
        assert_eq!(document_size_after(10, 6, 15), None);
        assert_eq!(document_size_after(usize::MAX, 1, usize::MAX), None);
    }

    #[test]
    fn retry_after_accepts_delta_seconds_and_http_dates() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("75"));
        assert_eq!(retry_after_secs(&headers), Some(75));

        let future = SystemTime::now() + Duration::from_mins(2);
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(future)).unwrap(),
        );
        assert!(matches!(retry_after_secs(&headers), Some(118..=120)));

        headers.insert(RETRY_AFTER, HeaderValue::from_static("not-a-date"));
        assert_eq!(retry_after_secs(&headers), None);
    }

    /// Mirrors Mastodon's `valid_activitypub_content_type?`: `activity+json`
    /// always passes; `ld+json` passes only with the `ActivityStreams` profile
    /// param; nothing else does — in particular plain `application/json`,
    /// which is how servers serve user-uploaded JSON.
    #[test]
    fn activitypub_content_type_matches_mastodon_rules() {
        assert!(is_activitypub_content_type("application/activity+json"));
        assert!(is_activitypub_content_type(
            "application/activity+json; charset=utf-8"
        ));
        assert!(is_activitypub_content_type(
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        ));
        // Multiple space-separated profiles, unquoted profile value.
        assert!(is_activitypub_content_type(
            "application/ld+json; profile=https://www.w3.org/ns/activitystreams"
        ));
        assert!(is_activitypub_content_type(
            "application/ld+json; charset=utf-8; profile=\"https://example.com/x \
             https://www.w3.org/ns/activitystreams\""
        ));

        assert!(!is_activitypub_content_type("application/ld+json"));
        assert!(!is_activitypub_content_type(
            "application/ld+json; profile=\"https://example.com/other\""
        ));
        assert!(!is_activitypub_content_type("application/json"));
        assert!(!is_activitypub_content_type("application/jrd+json"));
        assert!(!is_activitypub_content_type("text/html"));
        assert!(!is_activitypub_content_type(""));
    }

    #[test]
    fn webfinger_url_uses_http_for_hidden_services_only() {
        let clearnet: Acct = "user@remote.example".parse().unwrap();
        assert_eq!(
            webfinger_url(&clearnet),
            "https://remote.example/.well-known/webfinger?resource=acct:user@remote.example"
        );
        let onion: Acct = "user@xyz.onion".parse().unwrap();
        assert_eq!(
            webfinger_url(&onion),
            "http://xyz.onion/.well-known/webfinger?resource=acct:user@xyz.onion"
        );
    }

    /// Lane selection: hidden-service hosts ride their network's proxied
    /// client, `no_proxy` hosts the unproxied one, everything else the
    /// default. Compared by pool identity (`reqwest::Client` is a cheap
    /// handle; clones share one pool, separate builds do not).
    #[test]
    fn proxy_lanes_are_selected_per_destination() {
        // No proxies configured: every destination uses the default lane.
        let bare = FederationClient::new("plamenu-test", true).unwrap();
        let onion_url: Url = "http://xyz.onion/inbox".parse().unwrap();
        assert!(std::ptr::eq(
            bare.http_for(&onion_url),
            &raw const bare.http
        ));

        let client = FederationClient::new("plamenu-test", true)
            .unwrap()
            .with_proxies(&ProxyConfig {
                proxy_url: Some("socks5h://127.0.0.1:9050".into()),
                onion_proxy_url: Some("socks5h://127.0.0.1:9051".into()),
                i2p_proxy_url: None,
                no_proxy: vec!["Internal.Example".into()],
            })
            .unwrap();

        let clearnet: Url = "https://remote.example/inbox".parse().unwrap();
        let bypassed: Url = "https://internal.example/inbox".parse().unwrap();
        let i2p_url: Url = "http://xyz.i2p/inbox".parse().unwrap();

        // Onion gets its dedicated lane, distinct from the global one.
        assert!(std::ptr::eq(
            client.http_for(&onion_url),
            client.onion.as_ref().unwrap()
        ));
        // i2p has no override: falls back to the global proxy, but as its own
        // hidden lane (longer timeout), not the default client.
        assert!(std::ptr::eq(
            client.http_for(&i2p_url),
            client.i2p.as_ref().unwrap()
        ));
        // Clearnet uses the (proxied) default lane.
        assert!(std::ptr::eq(
            client.http_for(&clearnet),
            &raw const client.http
        ));
        // no_proxy matches the exact host case-insensitively and bypasses.
        assert!(std::ptr::eq(
            client.http_for(&bypassed),
            client.direct.as_ref().unwrap()
        ));
        assert!(std::ptr::eq(
            client.http_unproxied(),
            client.direct.as_ref().unwrap()
        ));

        // The recommended per-overlay configuration does not create a global
        // proxy or direct-bypass lane: clearnet keeps the original client and
        // its SafeResolver, while only onion traffic uses SOCKS.
        let overlay_only = FederationClient::new("plamenu-test", false)
            .unwrap()
            .with_proxies(&ProxyConfig {
                onion_proxy_url: Some("socks5h://127.0.0.1:9050".into()),
                ..ProxyConfig::default()
            })
            .unwrap();
        assert!(std::ptr::eq(
            overlay_only.http_for(&clearnet),
            &raw const overlay_only.http
        ));
        assert!(std::ptr::eq(
            overlay_only.http_for(&onion_url),
            overlay_only.onion.as_ref().unwrap()
        ));
        assert!(overlay_only.direct.is_none());
    }

    /// A proxy URL reqwest cannot parse fails at construction, not at first
    /// request.
    #[test]
    fn invalid_proxy_url_is_rejected_at_build() {
        let result = FederationClient::new("plamenu-test", true)
            .unwrap()
            .with_proxies(&ProxyConfig {
                proxy_url: Some("not a proxy url".into()),
                ..ProxyConfig::default()
            });
        assert!(matches!(result, Err(FederationError::InvalidUrl(_))));
    }

    #[test]
    fn invalid_proxy_error_does_not_echo_credentials() {
        let sentinel = "proxy-secret-sentinel";
        let result = FederationClient::new("plamenu-test", true)
            .unwrap()
            .with_proxies(&ProxyConfig {
                proxy_url: Some(format!("not a proxy url {sentinel}")),
                ..ProxyConfig::default()
            });
        let Err(error) = result else {
            panic!("invalid proxy unexpectedly accepted");
        };
        let message = error.to_string();
        assert!(
            !message.contains(sentinel),
            "proxy secret leaked in {message}"
        );
    }

    #[test]
    fn host_header_includes_only_non_default_ports() {
        let plain: Url = "https://remote.example/inbox".parse().unwrap();
        assert_eq!(host_header(&plain).unwrap(), "remote.example");
        // `Url::port()` is None for scheme-default ports, so 443 is elided.
        let default_port: Url = "https://remote.example:443/inbox".parse().unwrap();
        assert_eq!(host_header(&default_port).unwrap(), "remote.example");
        let custom: Url = "https://remote.example:8443/inbox".parse().unwrap();
        assert_eq!(host_header(&custom).unwrap(), "remote.example:8443");
    }

    #[test]
    fn authenticated_media_request_covers_exact_accept_and_target() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/ap/instance#main-key".into(),
        )
        .unwrap();
        let client = FederationClient::new("plamenu-test", true)
            .unwrap()
            .with_fetch_signer(signer);
        let url: Url = "https://audio.example:8443/listen/track?download=false"
            .parse()
            .unwrap();
        let request = client
            .media_get_request(&url, Some("bytes=10-19"), None, true)
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(request.headers()[ACCEPT], MEDIA_ACCEPT);
        assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=10-19");
        assert!(request.headers().contains_key("date"));
        assert!(
            request.headers()["signature"]
                .to_str()
                .unwrap()
                .contains("headers=\"(request-target) host date accept\"")
        );

        let mut headers = request.headers().clone();
        headers.insert("host", HeaderValue::from_static("audio.example:8443"));
        let prepared = PreparedVerification::from_get_request(
            "/listen/track?download=false",
            &headers,
            SystemTime::now(),
        )
        .unwrap();
        prepared.verify_pem(&pair.public_pem).unwrap();
    }

    #[test]
    fn request_scoped_fetch_signer_overrides_instance_actor() {
        let instance_pair = generate_keypair().unwrap();
        let instance = RequestSigner::from_pkcs8_pem(
            &instance_pair.private_pem,
            "https://plamenu.test/ap/instance#main-key".into(),
        )
        .unwrap();
        let recipient_pair = generate_keypair().unwrap();
        let recipient_key = "https://plamenu.test/users/alice#main-key";
        let recipient =
            RequestSigner::from_pkcs8_pem(&recipient_pair.private_pem, recipient_key.to_owned())
                .unwrap();
        let client = FederationClient::new("plamenu-test", true)
            .unwrap()
            .with_fetch_signer(instance);
        let url: Url = "https://remote.example/users/bob/statuses/1"
            .parse()
            .unwrap();

        let request = client
            .signed_get(&url, ACTIVITY_JSON, Some(&recipient))
            .unwrap()
            .build()
            .unwrap();
        let signature = request.headers()["signature"].to_str().unwrap();
        assert!(signature.contains(&format!("keyId=\"{recipient_key}\"")));
        assert!(!signature.contains("/ap/instance#main-key"));

        let mut headers = request.headers().clone();
        headers.insert("host", HeaderValue::from_static("remote.example"));
        let prepared = PreparedVerification::from_get_request(
            "/users/bob/statuses/1",
            &headers,
            SystemTime::now(),
        )
        .unwrap();
        prepared.verify_pem(&recipient_pair.public_pem).unwrap();
        assert!(prepared.verify_pem(&instance_pair.public_pem).is_err());
    }

    #[test]
    fn link_header_activitypub_alternate_is_resolved() {
        let base: Url = "https://flipboard.com/@newyorktimes/home/-/a-id-%2F0"
            .parse()
            .unwrap();
        let header = r#"<https://cdn.example/app.css>; rel="preload"; type="text/css", </users/newyorktimes/statuses/9N-2sEgeQCmZdQvJaePcwQ:a:3195393>; rel="alternate"; type="application/activity+json""#;

        assert_eq!(
            activitypub_alternate_from_link_header(header, &base).as_deref(),
            Some(
                "https://flipboard.com/users/newyorktimes/statuses/9N-2sEgeQCmZdQvJaePcwQ:a:3195393"
            )
        );
    }

    #[test]
    fn link_header_ld_json_profile_allows_quoted_semicolons() {
        let base: Url = "https://example.com/posts/1".parse().unwrap();
        let header = r#"</objects/1>; rel="alternate"; type="application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"""#;

        assert_eq!(
            activitypub_alternate_from_link_header(header, &base).as_deref(),
            Some("https://example.com/objects/1")
        );
    }

    #[test]
    fn html_activitypub_alternate_is_resolved_without_script_false_positives() {
        let base: Url = "https://flipboard.com/@newyorktimes/home/-/a-id-%2F0"
            .parse()
            .unwrap();
        let html = r#"
            <html>
              <head>
                <script>
                  '<link rel="alternate" type="application/activity+json" href="https://evil.example/object">';
                </script>
                <link rel="canonical" href="https://flipboard.com/article/example">
                <link rel="alternate noopener" type="application/activity+json" href="/users/newyorktimes/statuses/9N-2sEgeQCmZdQvJaePcwQ:a:3195393">
              </head>
            </html>
        "#;

        assert_eq!(
            activitypub_alternate_from_html(html, &base).as_deref(),
            Some(
                "https://flipboard.com/users/newyorktimes/statuses/9N-2sEgeQCmZdQvJaePcwQ:a:3195393"
            )
        );
    }

    #[tokio::test]
    async fn deliver_rechecks_redirects_and_resigns_for_final_path() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/users/alice#main-key".into(),
        )
        .unwrap();
        let client = FederationClient::new("plamenu-test", true).unwrap();
        let activity = json!({"id": "https://plamenu.test/activities/1", "type": "Follow"});

        let mut attempts = Vec::new();
        client
            .deliver_with(
                &activity,
                "https://origin.example/original/inbox",
                &signer,
                SignatureStyle::Cavage,
                |url, signed_headers, body| {
                    attempts.push(DeliveryAttempt {
                        url,
                        signed_headers,
                        body,
                    });
                    let response = if attempts.len() == 1 {
                        DeliveryResponse {
                            status: reqwest::StatusCode::FOUND,
                            location: Some("https://final.example/final/inbox?token=1".to_owned()),
                            retry_after_secs: None,
                        }
                    } else {
                        DeliveryResponse {
                            status: reqwest::StatusCode::ACCEPTED,
                            location: None,
                            retry_after_secs: None,
                        }
                    };
                    std::future::ready(Ok(response))
                },
            )
            .await
            .unwrap();

        assert_eq!(attempts.len(), 2);
        let original = &attempts[0];
        let final_attempt = &attempts[1];
        assert_eq!(
            original.url.as_str(),
            "https://origin.example/original/inbox"
        );
        assert_eq!(
            final_attempt.url.as_str(),
            "https://final.example/final/inbox?token=1"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&original.body).unwrap(),
            activity
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&final_attempt.body).unwrap(),
            activity
        );

        let final_headers = header_map(final_attempt);
        let prepared = PreparedVerification::from_request(
            "POST",
            &path_and_query(&final_attempt.url),
            &final_headers,
            &final_attempt.body,
            SystemTime::now(),
        )
        .unwrap();
        prepared.verify_pem(&pair.public_pem).unwrap();

        let replayed = PreparedVerification::from_request(
            "POST",
            &path_and_query(&original.url),
            &final_headers,
            &final_attempt.body,
            SystemTime::now(),
        )
        .unwrap();
        assert!(matches!(
            replayed.verify_pem(&pair.public_pem),
            Err(SignatureError::Invalid)
        ));
    }

    #[derive(Debug)]
    struct DeliveryAttempt {
        url: Url,
        signed_headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    fn header_map(attempt: &DeliveryAttempt) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.append(
            "host".parse::<HeaderName>().unwrap(),
            HeaderValue::from_str(attempt.url.host_str().unwrap()).unwrap(),
        );
        for (name, value) in &attempt.signed_headers {
            headers.append(
                name.to_ascii_lowercase().parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn private_literal_fetches_never_reach_the_connector() {
        let client = FederationClient::new("plamenu-test", false).unwrap();
        // Binding real listeners ensures refusal happens before any connection,
        // independently of TLS trust and DNS behavior.
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let listener = std::net::TcpListener::bind(bind).unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("https://{addr}/");
            assert!(matches!(
                client.fetch_page(&url, "text/html").await,
                Err(FederationError::PrivateAddress(_))
            ));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        assert!(matches!(
            client
                .fetch_page("https://[::ffff:127.0.0.1]/", "text/html")
                .await,
            Err(FederationError::PrivateAddress(_))
        ));
    }

    #[tokio::test]
    async fn delivery_redirects_reject_private_ipv6_before_second_post() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/ap/instance#main-key".into(),
        )
        .unwrap();
        let client = FederationClient::new("plamenu-test", false).unwrap();
        for target in [
            "https://[::1]/inbox",
            "https://[::ffff:127.0.0.1]/inbox",
            "https://[fc00::1]/inbox",
        ] {
            let mut attempts = 0;
            let result = client
                .deliver_with(
                    &json!({"type": "Follow"}),
                    "https://origin.example/inbox",
                    &signer,
                    SignatureStyle::Cavage,
                    |_, _, _| {
                        attempts += 1;
                        std::future::ready(Ok(DeliveryResponse {
                            status: reqwest::StatusCode::FOUND,
                            location: Some(target.to_owned()),
                            retry_after_secs: None,
                        }))
                    },
                )
                .await;
            assert!(matches!(result, Err(FederationError::PrivateAddress(_))));
            assert_eq!(attempts, 1);
        }
    }

    #[tokio::test]
    async fn deliver_rejects_insecure_redirect_before_second_post() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/users/alice#main-key".into(),
        )
        .unwrap();
        let client = FederationClient::new("plamenu-test", true).unwrap();

        let mut attempts = 0;
        let result = client
            .deliver_with(
                &json!({"type": "Follow"}),
                "https://origin.example/inbox",
                &signer,
                SignatureStyle::Cavage,
                |_url, _signed_headers, _body| {
                    attempts += 1;
                    std::future::ready(Ok(DeliveryResponse {
                        status: reqwest::StatusCode::FOUND,
                        location: Some("http://internal.example/inbox".to_owned()),
                        retry_after_secs: None,
                    }))
                },
            )
            .await;

        assert!(matches!(result, Err(FederationError::InvalidUrl(_))));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn deliver_stops_after_redirect_limit() {
        let pair = generate_keypair().unwrap();
        let signer = RequestSigner::from_pkcs8_pem(
            &pair.private_pem,
            "https://plamenu.test/users/alice#main-key".into(),
        )
        .unwrap();
        let client = FederationClient::new("plamenu-test", true).unwrap();

        let mut attempts = 0usize;
        let result = client
            .deliver_with(
                &json!({"type": "Follow"}),
                "https://origin.example/inbox",
                &signer,
                SignatureStyle::Cavage,
                |_url, _signed_headers, _body| {
                    attempts += 1;
                    std::future::ready(Ok(DeliveryResponse {
                        status: reqwest::StatusCode::TEMPORARY_REDIRECT,
                        location: Some("/loop".to_owned()),
                        retry_after_secs: None,
                    }))
                },
            )
            .await;

        match result {
            Err(FederationError::InvalidUrl(message)) => {
                assert!(message.contains("too many redirects"));
            }
            other => panic!("expected redirect limit error, got {other:?}"),
        }
        assert_eq!(attempts, DELIVERY_MAX_REDIRECTS + 1);
    }

    #[tokio::test]
    async fn double_knock_skips_rfc9421_when_not_asked() {
        let mut styles = Vec::new();
        let style = double_knock(false, |style| {
            styles.push(style);
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(style, SignatureStyle::Cavage);
        assert_eq!(styles, [SignatureStyle::Cavage]);
    }

    #[tokio::test]
    async fn double_knock_keeps_rfc9421_on_success() {
        let mut styles = Vec::new();
        let style = double_knock(true, |style| {
            styles.push(style);
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(style, SignatureStyle::Rfc9421);
        assert_eq!(styles, [SignatureStyle::Rfc9421]);
    }

    #[tokio::test]
    async fn double_knock_falls_back_after_refusal() {
        // 401: a draft-cavage-only server treating the request as unsigned.
        let mut styles = Vec::new();
        let style = double_knock(true, |style| {
            styles.push(style);
            std::future::ready(match style {
                SignatureStyle::Rfc9421 => Err(FederationError::Status(401)),
                SignatureStyle::Cavage => Ok(()),
            })
        })
        .await
        .unwrap();
        assert_eq!(style, SignatureStyle::Cavage);
        assert_eq!(styles, [SignatureStyle::Rfc9421, SignatureStyle::Cavage]);
    }

    #[tokio::test]
    async fn double_knock_falls_back_on_any_status_answer() {
        // Some header parsers 500 on the unfamiliar `sig1=:…:` shape — a
        // host must never lose deliveries to the knock itself.
        let mut styles = Vec::new();
        let result = double_knock(true, |style| {
            styles.push(style);
            std::future::ready(Err(FederationError::Status(500)))
        })
        .await;
        // Both attempts failed — the cavage error is what propagates.
        assert!(matches!(result, Err(FederationError::Status(500))));
        assert_eq!(styles, [SignatureStyle::Rfc9421, SignatureStyle::Cavage]);
    }

    #[tokio::test]
    async fn double_knock_does_not_retry_rate_limits() {
        // A 429 is pacing, not a signature verdict; an immediate cavage
        // retry would defeat the remote's Retry-After.
        let mut styles = Vec::new();
        let result = double_knock(true, |style| {
            styles.push(style);
            std::future::ready(Err(FederationError::RateLimited {
                retry_after_secs: Some(60),
            }))
        })
        .await;
        assert!(matches!(result, Err(FederationError::RateLimited { .. })));
        assert_eq!(styles, [SignatureStyle::Rfc9421]);
    }
}
