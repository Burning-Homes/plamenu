//! HTTP server assembly for Plamenu.

pub mod actions;
pub mod admin_log;
pub mod altcha;
pub mod archive;
pub mod archive_worker;
pub mod auth;
pub mod build_info;
pub mod bulk_import;
pub mod cache;
pub mod collections;
pub mod compose;
pub mod config;
pub mod containers;
pub mod conversations;
pub mod crypto;
pub mod crypto_gate;
pub mod delivery;
pub mod emoji;
pub mod entities;
pub mod error;
pub mod events;
pub mod federation;
pub mod filters;
pub mod followers_sync;
pub mod groups;
pub mod hidden_gate;
pub mod identity;
pub mod import_worker;
pub mod ingest;
pub mod instance_actor;
pub mod instance_policy;
pub mod key_store;
pub mod languages;
pub mod link_preview;
pub mod link_verify;
pub mod live_refresh;
pub mod local_identity;
pub mod mailer;
pub mod maintenance;
pub mod media_cleanup_worker;
pub mod media_gate;
pub mod media_processing;
pub mod media_transcode;
pub mod media_worker;
pub mod migration;
pub mod moderation;
pub mod note;
pub mod oauth_app;
pub mod owncast;
pub mod parent_fetch;
pub mod poll_expiry;
pub mod polls;
pub mod profile;
pub mod quote_verify;
pub mod rate_limit;
pub mod registration;
pub mod relays;
pub mod remote;
pub mod remote_history;
pub mod reply_fetch;
mod routes;
pub mod scheduled_status_publish;
pub mod self_destruct;
pub mod severance;
pub mod sign_in;
pub mod signed_fetch;
pub mod state;
pub mod statuses_cleanup;
pub mod storage;
pub mod streaming;
mod sync;
pub mod time_zones;
pub mod totp;
pub mod translation;
pub mod trends;
pub mod web;
pub mod web_push;
pub mod webhooks;
pub mod webxdc;
pub mod webxdc_realtime;
pub mod worker;
pub mod workers;

use axum::Router;

/// Per-request security choices shared between extractors / handlers and the
/// outer response-header middleware. The middleware reads them after rendering
/// to choose the matching HTML CSP.
#[derive(Default)]
struct RequestSecurityChoices {
    direct_remote_media: std::sync::atomic::AtomicBool,
    oauth_form_action: std::sync::OnceLock<String>,
}

#[derive(Clone, Default)]
pub(crate) struct RequestSecurityContext(std::sync::Arc<RequestSecurityChoices>);

impl RequestSecurityContext {
    fn allow_direct_remote_media(&self) {
        self.0
            .direct_remote_media
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn direct_remote_media_allowed(&self) -> bool {
        self.0
            .direct_remote_media
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Allows a validated OAuth callback source on the authorization page's
    /// `form-action`. Chromium applies that directive to redirects after a
    /// form POST, so a self-only policy otherwise blocks the authorization
    /// server's redirect back to a web client.
    pub(crate) fn allow_oauth_form_action(&self, source: String) {
        let _ = self.0.oauth_form_action.set(source);
    }

    fn oauth_form_action(&self) -> Option<&str> {
        self.0.oauth_form_action.get().map(String::as_str)
    }
}
use axum::extract::Request;
pub use config::Config;
pub use state::AppState;
use tower_http::trace::TraceLayer;

pub use build_info::{BUILD_INFO, BuildInfo};
/// Publicly reported product version. Release builds expose only `SemVer`;
/// non-release builds also expose their channel, build, and source identity.
pub const VERSION: &str = build_info::PUBLIC_VERSION;
/// Authored `SemVer` used for release/update comparisons.
pub const PACKAGE_VERSION: &str = build_info::PACKAGE_VERSION;
/// Exact build identity for administrative and diagnostic output.
pub const FULL_VERSION: &str = build_info::FULL_VERSION;
pub const SOURCE_URL: &str = "https://codefloe.com/plamenu/plamenu";

/// The Mastodon API version targeted by Plamenu.
pub const MASTODON_COMPAT_VERSION: &str = "4.7.1";

/// The version string reported through the Mastodon-compatible API.
///
/// Clients gate features on the leading Mastodon version, so we advertise the
/// API level we target and identify ourselves in the compat suffix (the same
/// convention `GoToSocial` and Pleroma use).
#[must_use]
pub fn compat_version() -> String {
    format!("{MASTODON_COMPAT_VERSION} (compatible; Plamenu {VERSION})")
}

/// The tracing span for one HTTP request. Unlike tower-http's default span it
/// records only the URI *path*, never the query string: several endpoints carry
/// bearer-like secrets there — the streaming `access_token`, password-reset and
/// e-mail-confirmation tokens, and signup invite codes — and the default span
/// records the whole URI, leaking them verbatim under `RUST_LOG=debug`.
fn make_request_span(request: &Request) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        version = ?request.version(),
    )
}

/// Response paths that carry credentials, authorization codes, reset or
/// confirmation tokens, or 2FA challenges, or that render a signed-in user's
/// private settings. Their responses must never be cached by a browser or a
/// shared proxy. Prefixes match on a path-segment boundary so `/login` does not
/// also match an unrelated `/loginfoo`.
fn is_credential_path(path: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "/login",        // password + security-key sign-in
        "/signup",       // registration (may carry an invite token)
        "/logout",       // session termination
        "/auth",         // password reset + e-mail confirmation
        "/oauth",        // authorize consent, token, revoke
        "/api/v3/user",  // Lemmy-compatible login, tokens, and account settings
        "/settings",     // private settings pages (security, applications, …)
        "/web/settings", // their POST mutation actions
    ];
    PREFIXES
        .iter()
        .any(|prefix| match path.strip_prefix(prefix) {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        })
}

/// The full content-security policy, **enforced** on every HTML response
/// since the 2026-07-30/31 staging report-only pass. That window surfaced
/// exactly one violation class — the blurhash placeholder painted through
/// `canvas.toDataURL()`, which now encodes to a `blob:` URL instead — and a
/// scripted browser pass over home, public, profiles, attachments, video
/// playback, the composer, login, and the admin console came back clean.
///
/// Shape notes: `default-src 'none'` with every fetch directive named, so a
/// future feature that forgets to declare an origin surfaces as a console
/// error and a `/csp-report` entry, not a silently-permitted request. `blob:`
/// in `script-src`/`worker-src` is hls.js's demuxer worker, in `img-src` the
/// composer's local upload previews and blurhash placeholders, in
/// `media-src` MSE playback. `data:` is deliberately absent everywhere. The
/// single inline bootstrap is admitted by sha256 of
/// [`web::layout::BOOTSTRAP_SCRIPT`] — a hash, not a nonce, so both header
/// variants are per-process constants and public pages stay cacheable. The
/// direct-media variant admits remote HTTPS images and media; it is selected only
/// after the browser session resolves a user whose direct-media preference is
/// on. Anonymous and opted-out pages retain the same-origin policy.
/// The second `style-src` hash admits the exact stylesheet injected by the
/// pinned ALTCHA 3.2.3 widget. This keeps the signup widget functional without
/// opening every page to arbitrary inline styles; an ALTCHA upgrade must update
/// the hash as well. `style-src-attr 'unsafe-inline'` covers the three validated
/// computed `style=` attributes (role colour, role swatch, poll bar). CSP
/// violation reporting is optional:
/// its directive is emitted only when the default-off `csp_reporting` config
/// flag deliberately enables the matching route.
fn content_security_policy(
    allow_direct_media: bool,
    webxdc_domain: &str,
    csp_reporting: bool,
    oauth_form_action: Option<&str>,
) -> axum::http::HeaderValue {
    use base64::Engine;
    use sha2::Digest;
    let hash = base64::engine::general_purpose::STANDARD
        .encode(sha2::Sha256::digest(web::layout::BOOTSTRAP_SCRIPT));
    let media_sources = if allow_direct_media {
        "'self' blob: https:"
    } else {
        "'self' blob:"
    };
    let reporting = if csp_reporting {
        "; report-uri /csp-report"
    } else {
        ""
    };
    let form_action = oauth_form_action.map_or_else(
        || "'self'".to_owned(),
        |callback| format!("'self' {callback}"),
    );
    axum::http::HeaderValue::from_str(&format!(
        "default-src 'none'; script-src 'self' blob: 'sha256-{hash}'; \
             style-src 'self' 'sha256-ZgqGuQlekW98cv0XQjYUGCLTvc3q5MkU+2SkqlFGoTM='; \
             style-src-attr 'unsafe-inline'; \
             img-src {media_sources}; media-src {media_sources}; connect-src 'self'; \
             worker-src 'self' blob:; frame-src https://*.{webxdc_domain}; \
             manifest-src 'self'; form-action {form_action}; \
             base-uri 'none'; frame-ancestors 'none'; object-src 'none'{reporting}"
    ))
    .expect("a base64 hash is always a valid header value")
}

/// Global response hardening. Denies framing of every HTML page (no first-party
/// surface is meant to be embedded, so this blocks clickjacking of login, OAuth
/// consent, and settings actions) and marks credential-bearing responses
/// `no-store` so browsers and shared proxies never retain them. Wired in
/// application middleware so direct deployments are protected regardless of the
/// reverse proxy.
async fn security_headers(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut request: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header};

    let sensitive = is_credential_path(request.uri().path());
    let security = RequestSecurityContext::default();
    request.extensions_mut().insert(security.clone());
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // Content-type sniffing is refused everywhere, not just on HTML. It
    // matters most on `/media/{file}`, which serves attacker-supplied bytes
    // under a server-chosen content type (and `application/octet-stream` for
    // anything unrecognised): without this a browser may sniff such a
    // response into HTML and run it on our own origin.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.starts_with("text/html"));
    let webxdc_runtime = headers.remove("x-plamenu-webxdc-runtime").is_some();
    // Keep paths and query strings — password-reset and confirmation links
    // carry their token there — out of cross-origin referrers. The untrusted
    // Webxdc origin has the stricter contract that it sends no referrer at all.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static(if webxdc_runtime {
            "no-referrer"
        } else {
            "strict-origin-when-cross-origin"
        }),
    );
    if is_html && webxdc_runtime {
        // Asset loaders (including Three.js ImageBitmapLoader) fetch local
        // blob/data URLs before decoding them. These schemes do not contact
        // the network; external destinations remain excluded from connect-src.
        let policy = HeaderValue::from_str(&format!(
            "default-src 'self' data: blob:; script-src 'self' 'unsafe-inline' \
             'unsafe-eval' blob:; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; media-src 'self' data: blob:; \
             font-src 'self' data:; connect-src 'self' blob: data:; worker-src 'self' blob:; \
             child-src blob:; frame-ancestors https://{}; object-src 'none'; \
             base-uri 'self'; form-action 'none'",
            state.config.domain
        ))
        .expect("validated domains form valid CSP values");
        headers.insert(header::CONTENT_SECURITY_POLICY, policy);
    } else if is_html {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            content_security_policy(
                security.direct_remote_media_allowed(),
                &state.config.webxdc_domain(),
                state.config.csp_reporting,
                security.oauth_form_action(),
            ),
        );
    }
    if sensitive {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    }
    response
}

/// Builds the complete application router.
pub fn build_router(state: AppState) -> Router {
    routes::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state,
            security_headers,
        ))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that appends every log line into a shared buffer the test
    /// can read back.
    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBuffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn request_span_records_path_but_not_query_secrets() {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let request = axum::http::Request::builder()
                .method("GET")
                .uri("/api/v1/streaming?access_token=SUPER_SECRET_TOKEN")
                .body(axum::body::Body::empty())
                .unwrap();
            let span = super::make_request_span(&request);
            let _entered = span.enter();
            tracing::debug!("handling request");
        });

        let logged = buffer.contents();
        assert!(
            logged.contains("path=/api/v1/streaming"),
            "span should record the request path, got: {logged}"
        );
        assert!(
            !logged.contains("SUPER_SECRET_TOKEN"),
            "span must not record query-string secrets, got: {logged}"
        );
        assert!(
            !logged.contains("access_token"),
            "span must not record the query string at all, got: {logged}"
        );
    }

    #[test]
    fn credential_paths_are_classified_no_store() {
        for path in [
            "/login",
            "/login/challenge",
            "/login/webauthn/options",
            "/signup",
            "/logout",
            "/auth/password/new",
            "/auth/password/edit",
            "/auth/confirmation",
            "/oauth/authorize",
            "/oauth/token",
            "/oauth/revoke",
            "/settings",
            "/settings/security",
            "/settings/applications/new",
            "/web/settings/webauthn",
        ] {
            assert!(
                super::is_credential_path(path),
                "{path} should be sensitive"
            );
        }
    }

    #[test]
    fn public_paths_are_not_classified_no_store() {
        for path in [
            "/",
            "/public",
            "/explore",
            "/loginfoo",       // not a `/login` sub-path
            "/settingsful",    // not a `/settings` sub-path
            "/assets/app.css", // cacheable static asset
            "/media/abc123.png",
            "/@alice",
            "/users/alice/outbox",
            "/health",
        ] {
            assert!(
                !super::is_credential_path(path),
                "{path} should not be sensitive"
            );
        }
    }
}
