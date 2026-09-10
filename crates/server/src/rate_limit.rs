//! Mastodon-style rate limiting — the `Rack::Attack` throttle set, with the
//! same fixed-window semantics Mastodon counts in Redis. Limits are
//! operator-editable (`instance_settings`); periods are fixed constants
//! matching Mastodon's.
//!
//! Counters live in one of two places. The security-sensitive
//! buckets — credential brute force, account/app creation, token minting,
//! paid translation ([`Bucket::durable_key`]) — count in Postgres
//! (`rate_limit_windows`), so their budgets survive restarts, are shared by
//! every app instance, and *fail closed* when the store is unreachable. The
//! high-volume load-shedding buckets (general API, paging, remote ingress,
//! per-account web admission) count in-process, where a restart clearing them
//! is harmless and a per-request DB round trip would defeat their purpose;
//! they fail open.
//!
//! Most buckets are evaluated by the [`gate`] middleware from the request
//! line alone. The e-mail-keyed buckets (login attempts, password resets)
//! need the parsed form body, so their handlers call [`check_email`] instead.

use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use plamenu_db::instance_settings::InstanceSettings;
use plamenu_db::{PgPool, oauth};

use crate::AppState;
use crate::auth::hash_secret;
use crate::error::ApiError;

const SHARD_COUNT: usize = 8;
/// How often a shard clears out windows from past periods.
const SWEEP_INTERVAL: Duration = Duration::from_mins(5);
/// How long a bearer-token → identity resolution stays cached.
const TOKEN_CACHE_TTL: Duration = Duration::from_mins(1);
/// Hard ceiling on cached token→identity entries; eviction keeps the map at or
/// below this even under a flood of unique fresh tokens.
const TOKEN_CACHE_MAX: usize = 10_000;

/// Per-account request budget for expensive first-party web maintenance
/// operations (full-archive requests, and the CSV export downloads that walk
/// entire relationship datasets), per
/// [`Bucket::WebMaintenance`] period. Not an operator tuning knob: it sits far
/// above any legitimate use — a real account requests an archive roughly once
/// every six days (the cooldown) and downloads a handful of exports — so it is
/// a constant, unlike the operator-set API budgets. Its only job is to stop a
/// malicious or buggy authenticated client from hammering these routes, where
/// each attempt otherwise costs real database work up front.
pub const WEB_MAINTENANCE_BUDGET: u32 = 30;

/// Per-account and per-IP request budget for CSV import uploads, per
/// [`Bucket::ImportUpload`] period. Each accepted upload buffers and parses a
/// body of up to ~21 MiB and can persist up to 20,000 rows, so an unthrottled
/// authenticated client (or a burst from one IP cycling accounts) could force
/// large repeated allocations and DB writes before the per-account
/// unfinished-import cap refuses them. Like
/// [`WEB_MAINTENANCE_BUDGET`] this is a fixed admission ceiling, not an operator
/// tuning knob: it sits far above legitimate use — a real member imports their
/// follows/blocks a handful of times — so its only job is to turn spam away
/// *before* the body is buffered.
pub const IMPORT_UPLOAD_BUDGET: u32 = 30;

/// Per-account and per-IP request budget for local group creation, per
/// [`Bucket::GroupCreate`] period. Group creation is open to every account by
/// default and mints an RSA-2048 actor key per group, so an unthrottled client
/// could submit many concurrent unique names and monopolize the crypto threads
/// before the per-account group quota (`groups::MAX_GROUPS_PER_ACCOUNT`) refuses
/// them. Like the other fixed web budgets this sits far above
/// legitimate use — a real member makes a handful of groups — so its only job is
/// to turn a burst away before key generation.
pub const GROUP_CREATE_BUDGET: u32 = 10;

/// One throttle rule. Names, keys, limits and periods mirror Mastodon's
/// rate-limit buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// All of `/api/*` per user (Mastodon: 1500 / 5 min).
    AuthenticatedApi,
    /// All of `/api/*` per token (300 / 5 min).
    PerTokenApi,
    /// All of `/api/*` per IP without a resolvable user (300 / 5 min).
    UnauthenticatedApi,
    /// `POST /api/v{n}/media` per user (30 / 30 min).
    ApiMedia,
    /// `DELETE /api/v1/statuses/{id}` + `POST …/unreblog` per user
    /// (30 / 30 min).
    ApiDelete,
    /// `POST /api/v1/accounts` per IP (5 / 30 min).
    ApiSignUp,
    /// `POST /api/v1/apps` per IP (5 / 10 min).
    AppRegistrations,
    /// Requests carrying paging params, per user (300 / 15 min).
    AuthenticatedPaging,
    /// Requests carrying paging params, per IP (300 / 15 min).
    UnauthenticatedPaging,
    /// Credential-checking POSTs per IP (25 / 5 min).
    LoginAttemptsIp,
    /// Credential-checking POSTs per e-mail (25 / 1 h).
    LoginAttemptsEmail,
    /// Password-reset requests per e-mail (5 / 30 min).
    PasswordResetsEmail,
    /// `POST /signup` per IP (25 / 5 min).
    SignUpWeb,
    /// Federation inbox, signed-fetch, and remote-media proxy requests per IP.
    /// These routes can initiate outbound dereferences and therefore need an
    /// admission budget even though they are outside `/api/*`.
    RemoteIngress,
    /// Status translations per account that *reach the backend* — cache hits
    /// are unmetered, so this budgets actual backend spend. Plamenu-specific
    /// (Mastodon has no translate throttle); checked inside the translate
    /// path via [`check_translation`], not the middleware.
    TranslateBackend,
    /// Expensive first-party web maintenance operations per account (currently
    /// full-archive requests). Plamenu-specific admission control checked inside
    /// the handler via [`check_web_maintenance`], not the middleware, because the
    /// web surface is session- rather than bearer-authenticated.
    WebMaintenance,
    /// `POST /oauth/token` and `POST /oauth/revoke` per IP. These live outside
    /// `/api/*` and outside the login-path set, so they otherwise drew from no
    /// bucket at all: an anonymous caller could mint or revoke tokens as fast as
    /// it liked, each request costing a Postgres client lookup before any work
    /// Sharing the login-attempts budget gives them a cheap
    /// per-IP admission ceiling in the middleware, before the handler resolves
    /// the client.
    TokenEndpoint,
    /// CSV import uploads, per account and per IP. Plamenu-specific admission
    /// control checked inside the handler via [`check_import_upload`] — before
    /// the ~21 MiB multipart body is buffered — because the web upload surface is
    /// session- rather than bearer-authenticated, so the middleware [`gate`]
    /// cannot key it by account.
    ImportUpload,
    /// Local group creation, per account and per IP. Plamenu-specific admission
    /// control checked inside the handler via [`check_group_create`] — before
    /// the RSA actor key is generated — because the web group-creation surface is
    /// session- rather than bearer-authenticated.
    GroupCreate,
}

impl Bucket {
    fn period_secs(self) -> u64 {
        match self {
            Self::AuthenticatedApi
            | Self::PerTokenApi
            | Self::UnauthenticatedApi
            | Self::LoginAttemptsIp
            | Self::SignUpWeb
            | Self::RemoteIngress
            | Self::TokenEndpoint => 300,
            Self::ApiMedia
            | Self::ApiDelete
            | Self::ApiSignUp
            | Self::PasswordResetsEmail
            | Self::WebMaintenance
            | Self::ImportUpload
            | Self::GroupCreate => 1800,
            Self::AppRegistrations => 600,
            Self::AuthenticatedPaging | Self::UnauthenticatedPaging => 900,
            Self::LoginAttemptsEmail | Self::TranslateBackend => 3600,
        }
    }

    /// The operator-set request count for this bucket (floored at 1 so a
    /// mis-set 0 cannot brick the API outright).
    fn limit(self, settings: &InstanceSettings) -> u32 {
        let count = match self {
            Self::AuthenticatedApi => settings.rate_limit_authenticated_api,
            Self::PerTokenApi => settings.rate_limit_per_token_api,
            Self::UnauthenticatedApi | Self::RemoteIngress => {
                settings.rate_limit_unauthenticated_api
            }
            Self::ApiMedia => settings.rate_limit_api_media,
            Self::ApiDelete => settings.rate_limit_api_delete,
            Self::ApiSignUp => settings.rate_limit_api_sign_up,
            Self::AppRegistrations => settings.rate_limit_app_registrations,
            Self::AuthenticatedPaging | Self::UnauthenticatedPaging => settings.rate_limit_paging,
            Self::LoginAttemptsIp | Self::LoginAttemptsEmail | Self::TokenEndpoint => {
                settings.rate_limit_login_attempts
            }
            Self::PasswordResetsEmail => settings.rate_limit_password_resets,
            Self::SignUpWeb => settings.rate_limit_sign_up_web,
            Self::TranslateBackend => settings.translation_user_rate_limit_per_hour,
            Self::WebMaintenance => i32::try_from(WEB_MAINTENANCE_BUDGET).unwrap_or(i32::MAX),
            Self::ImportUpload => i32::try_from(IMPORT_UPLOAD_BUDGET).unwrap_or(i32::MAX),
            Self::GroupCreate => i32::try_from(GROUP_CREATE_BUDGET).unwrap_or(i32::MAX),
        };
        u32::try_from(count).unwrap_or(1).max(1)
    }

    /// The storage name of a *durable* bucket — one whose counter lives in
    /// Postgres (`rate_limit_windows`) so it survives restarts and is shared
    /// across app instances. Durable buckets are the
    /// security-sensitive ones: credential brute force, account/app creation,
    /// token minting, and paid translation spend. `None` means the bucket is a
    /// load-shedding budget (general API, paging, remote ingress, per-account
    /// web admission) where a per-process window is the point — those stay
    /// in-memory and never cost a DB round trip.
    fn durable_key(self) -> Option<&'static str> {
        match self {
            Self::LoginAttemptsIp => Some("login_ip"),
            Self::LoginAttemptsEmail => Some("login_email"),
            Self::PasswordResetsEmail => Some("password_resets"),
            Self::ApiSignUp => Some("api_sign_up"),
            Self::SignUpWeb => Some("sign_up_web"),
            Self::AppRegistrations => Some("app_registrations"),
            Self::TokenEndpoint => Some("token_endpoint"),
            Self::TranslateBackend => Some("translate_backend"),
            Self::AuthenticatedApi
            | Self::PerTokenApi
            | Self::UnauthenticatedApi
            | Self::ApiMedia
            | Self::ApiDelete
            | Self::AuthenticatedPaging
            | Self::UnauthenticatedPaging
            | Self::RemoteIngress
            | Self::WebMaintenance
            | Self::ImportUpload
            | Self::GroupCreate => None,
        }
    }
}

/// What a bucket counts by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Identity {
    User(i64),
    Token(i64),
    Ip(IpAddr),
    Email(String),
}

impl Identity {
    /// The storage rendering for a durable counter row. E-mail identities are
    /// hashed before they touch the table: the login/reset buckets count
    /// attempts against addresses that need not belong to any account here, and
    /// a rate table is no place to accumulate third-party PII. IPs and local
    /// ids stay readable — they are already operational data and legibility
    /// helps an operator inspecting the table.
    fn durable_key(&self) -> String {
        match self {
            Self::User(id) => format!("user:{id}"),
            Self::Token(id) => format!("token:{id}"),
            Self::Ip(ip) => format!("ip:{ip}"),
            Self::Email(email) => format!("email:{}", hash_secret(email)),
        }
    }
}

/// The result of counting a request against one bucket.
#[derive(Debug, Clone, Copy)]
struct Outcome {
    limit: u32,
    remaining: u32,
    reset_epoch: u64,
    throttled: bool,
}

#[derive(Debug)]
struct Window {
    period_index: u64,
    count: u32,
}

struct Shard {
    windows: HashMap<(Bucket, Identity), Window>,
    last_sweep: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenIdentity {
    token_id: i64,
    user_id: Option<i64>,
}

/// One lock per in-flight token→identity resolution, keyed by token hash, so
/// concurrent lookups of the same token coalesce onto one DB round trip.
type TokenInflightMap = HashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// The result of a token-cache probe: a hit carries the resolution itself
/// (`None` = a cached "no such token"); a miss means absent or stale.
enum Cached {
    Hit(Option<TokenIdentity>),
    Miss,
}

/// The shared counter state, one per [`AppState`].
pub struct RateLimiter {
    shards: Vec<Mutex<Shard>>,
    token_identities: Mutex<HashMap<String, (Instant, Option<TokenIdentity>)>>,
    /// Single-flight registry for token resolution. A plain
    /// `Mutex` (never held across an await) guards the map; the async lock it
    /// hands out is held across the miss path so identical concurrent requests
    /// serialize and then re-read the cache the winner filled.
    token_inflight: Arc<Mutex<TokenInflightMap>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| {
                    Mutex::new(Shard {
                        windows: HashMap::new(),
                        last_sweep: Instant::now(),
                    })
                })
                .collect(),
            token_identities: Mutex::new(HashMap::new()),
            token_inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// A claim on the in-flight slot for one token hash. Carries the per-hash
/// serialization lock and removes the map entry when dropped — on every exit
/// path — so a fallible or cancelled resolution cannot leak a stale entry.
struct TokenInflightGuard {
    map: Arc<Mutex<TokenInflightMap>>,
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl TokenInflightGuard {
    fn lock(&self) -> &tokio::sync::Mutex<()> {
        &self.lock
    }
}

impl Drop for TokenInflightGuard {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl RateLimiter {
    /// Counts one request against `bucket` for `identity` — `Rack::Attack`'s
    /// fixed window: the counter belongs to the period slot `now / period`
    /// and resets on the period boundary.
    fn check(&self, bucket: Bucket, identity: Identity, limit: u32, now_epoch: u64) -> Outcome {
        let period = bucket.period_secs();
        let period_index = now_epoch / period;
        let reset_epoch = (period_index + 1) * period;

        let mut shard = self.shards[shard_index(bucket, &identity)]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if shard.last_sweep.elapsed() >= SWEEP_INTERVAL {
            shard.windows.retain(|(bucket, _), window| {
                (window.period_index + 1) * bucket.period_secs() > now_epoch
            });
            shard.last_sweep = Instant::now();
        }
        let window = shard.windows.entry((bucket, identity)).or_insert(Window {
            period_index,
            count: 0,
        });
        if window.period_index != period_index {
            window.period_index = period_index;
            window.count = 0;
        }
        window.count = window.count.saturating_add(1);
        Outcome {
            limit,
            remaining: limit.saturating_sub(window.count),
            reset_epoch,
            throttled: window.count > limit,
        }
    }

    /// [`Self::check`] for a durable bucket: the counter lives in Postgres, so
    /// it survives restarts and is shared across instances. The
    /// caller decides what a `DbError` means; every current caller fails
    /// *closed*, because a durable bucket guards a route whose handler needs
    /// the database anyway — refusing early gives up nothing and never leaves
    /// a brute-force path unmetered during database pressure.
    async fn check_durable(
        pool: &PgPool,
        bucket: Bucket,
        identity: &Identity,
        limit: u32,
        now_epoch: u64,
    ) -> Result<Outcome, plamenu_db::DbError> {
        let key = bucket
            .durable_key()
            .expect("check_durable requires a durable bucket");
        let period = bucket.period_secs();
        let period_index = now_epoch / period;
        let reset_epoch = (period_index + 1) * period;
        // `expires_at` only drives the maintenance sweep — correctness never
        // depends on it — so an unrepresentable instant may harmlessly fall
        // back to "now".
        let expires_at =
            time::OffsetDateTime::from_unix_timestamp(i64::try_from(reset_epoch).unwrap_or(0))
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let count = plamenu_db::rate_limit::increment(
            pool,
            key,
            &identity.durable_key(),
            i64::try_from(period_index).unwrap_or(i64::MAX),
            expires_at,
        )
        .await?;
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        Ok(Outcome {
            limit,
            remaining: limit.saturating_sub(count),
            reset_epoch,
            throttled: count > limit,
        })
    }

    /// Whether a further request against `bucket` for `identity` in the current
    /// window *would* be throttled — a read-only peek that does not consume
    /// budget. Used to admit or shed work before spending an expensive step
    /// skip the token DB lookup once an IP is over budget.
    fn would_throttle(
        &self,
        bucket: Bucket,
        identity: Identity,
        limit: u32,
        now_epoch: u64,
    ) -> bool {
        let period_index = now_epoch / bucket.period_secs();
        let shard = self.shards[shard_index(bucket, &identity)]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match shard.windows.get(&(bucket, identity)) {
            // `check` throttles once the post-increment count exceeds `limit`,
            // so a stored count already at the limit means the next one trips.
            Some(window) if window.period_index == period_index => window.count >= limit,
            _ => false,
        }
    }

    /// The cached identity for `hash` if the entry is present and still fresh.
    fn cached_token(&self, hash: &str) -> Cached {
        let cache = self
            .token_identities
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache
            .get(hash)
            .filter(|(cached_at, _)| cached_at.elapsed() < TOKEN_CACHE_TTL)
            .map_or(Cached::Miss, |(_, identity)| Cached::Hit(*identity))
    }

    /// Claims the in-flight slot for `hash`. Hold the returned guard's lock
    /// across the miss path; identical concurrent requests serialize on it and
    /// then re-read the cache the winner filled.
    fn token_inflight_guard(&self, hash: &str) -> TokenInflightGuard {
        let lock = Arc::clone(
            self.token_inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(hash.to_owned())
                .or_default(),
        );
        TokenInflightGuard {
            map: Arc::clone(&self.token_inflight),
            key: hash.to_owned(),
            lock,
        }
    }

    /// Resolves a bearer token to its (token id, user id) pair through a
    /// short-lived cache, so the hot API path costs one indexed lookup per
    /// token per minute instead of per request. Nothing here enforces auth —
    /// an invalid token simply counts against the unauthenticated bucket,
    /// like Mastodon.
    async fn token_identity(&self, pool: &PgPool, bearer: &str) -> Option<TokenIdentity> {
        let hash = hash_secret(bearer);
        self.resolve_coalesced(hash.clone(), || async {
            oauth::find_active_token(pool, &hash).await.map(|token| {
                token.map(|token| TokenIdentity {
                    token_id: token.id,
                    user_id: token.user_id,
                })
            })
        })
        .await
    }

    /// The cache + single-flight core of [`Self::token_identity`], with the DB
    /// call injected so it can be exercised without a database. Concurrent
    /// callers for the same `hash` run `resolve` at most once.
    async fn resolve_coalesced<F, Fut>(&self, hash: String, resolve: F) -> Option<TokenIdentity>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<TokenIdentity>, plamenu_db::DbError>>,
    {
        if let Cached::Hit(identity) = self.cached_token(&hash) {
            return identity;
        }
        // Miss: coalesce identical concurrent lookups onto one resolution.
        let inflight = self.token_inflight_guard(&hash);
        let _guard = inflight.lock().lock().await;
        // The winner may have filled the cache while we waited for the lock.
        if let Cached::Hit(identity) = self.cached_token(&hash) {
            return identity;
        }
        let resolved = match resolve().await {
            Ok(identity) => identity,
            // A database hiccup must not get cached as "no such token".
            Err(err) => {
                tracing::error!(error = %err, "rate limiter failed to resolve token");
                return None;
            }
        };
        let mut cache = self
            .token_identities
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A flood of unique bogus tokens is all fresh, so a stale-only sweep
        // frees nothing and the map would grow past `TOKEN_CACHE_MAX`; bound it
        // for real.
        crate::cache::bounded_ttl_insert(
            &mut cache,
            hash,
            resolved,
            Instant::now(),
            TOKEN_CACHE_TTL,
            TOKEN_CACHE_MAX,
        );
        resolved
    }
}

fn shard_index(bucket: Bucket, identity: &Identity) -> usize {
    let mut hasher = std::hash::DefaultHasher::new();
    bucket.hash(&mut hasher);
    identity.hash(&mut hasher);
    usize::try_from(hasher.finish()).unwrap_or(0) % SHARD_COUNT
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Mastodon throttles v6 clients by their /64 (a home allocation), not the
/// full address.
fn throttleable_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & (u128::MAX << 64)).into()),
    }
}

/// What the request line tells us about which buckets apply.
// Independent flags, one per throttle rule; an enum would misrepresent that
// several can hold at once.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, PartialEq)]
struct RequestClass {
    api: bool,
    paging: bool,
    media_post: bool,
    delete_action: bool,
    api_sign_up: bool,
    app_registration: bool,
    login_post: bool,
    web_sign_up: bool,
    remote_ingress: bool,
    token_endpoint: bool,
}

impl RequestClass {
    fn any(&self) -> bool {
        self.api
            || self.paging
            || self.login_post
            || self.web_sign_up
            || self.remote_ingress
            || self.token_endpoint
    }

    /// Whether this request must be *refused* rather than admitted unmetered
    /// when the rate limiter cannot do its job (settings unavailable) — the
    /// explicit per-route fail-open/closed decision. Credential
    /// checking, account/app creation and token minting fail closed: they are
    /// exactly the brute-force surfaces the limiter exists for, and their
    /// handlers need the database anyway, so refusing early gives up nothing.
    /// Everything else (general API, paging, remote ingress) fails open —
    /// availability wins for traffic whose budget is load shedding, not
    /// security.
    fn fails_closed(&self) -> bool {
        self.login_post
            || self.web_sign_up
            || self.token_endpoint
            || self.api_sign_up
            || self.app_registration
    }
}

fn classify(method: &Method, path: &str, query: Option<&str>) -> RequestClass {
    let api = path.starts_with("/api/");
    let post = method == Method::POST;
    RequestClass {
        api,
        paging: query.is_some_and(has_paging_params),
        media_post: post && is_media_path(path),
        delete_action: (method == Method::DELETE && is_status_path(path))
            || (post && is_unreblog_path(path)),
        api_sign_up: post && path == "/api/v1/accounts",
        app_registration: post && path == "/api/v1/apps",
        login_post: post
            && matches!(
                path,
                "/login"
                    | "/login/challenge"
                    | "/oauth/authorize"
                    | "/auth/password"
                    // The reset-completion POST validates a token and (on a live
                    // one) spends an Argon2 hash; give it the same per-IP
                    // admission ceiling as the other credential POSTs so an
                    // anonymous caller cannot flood it.
                    | "/auth/password/edit"
                    // The WebAuthn ceremony POSTs are anonymous credential
                    // checks like `/login/challenge`: options mints a stored
                    // challenge per call, finish verifies an assertion.
                    | "/login/webauthn/options"
                    | "/login/webauthn"
                    | "/oauth/authorize/webauthn/options"
                    | "/oauth/authorize/webauthn"
                    // Confirmation resend rotates the token and sends mail;
                    // it shares the credential admission ceiling so a leaked
                    // unconfirmed-account token cannot mail-bomb the address.
                    | "/api/v1/emails/confirmations"
            ),
        web_sign_up: post && path == "/signup",
        // `/interact` submits trigger an outbound WebFinger for an anonymous
        // visitor, so they draw from the same admission budget.
        remote_ingress: post && matches!(path, "/inbox" | "/actor/inbox" | "/interact")
            || (post && path.starts_with("/users/") && path.ends_with("/inbox"))
            || (!post && path.starts_with("/users/"))
            || path.starts_with("/media/proxy/")
            || path.starts_with("/media/hls/"),
        // Anonymous OAuth token minting/revocation lives outside `/api/*`; give
        // it a per-IP admission ceiling before the handler's client lookup.
        token_endpoint: post && matches!(path, "/oauth/token" | "/oauth/revoke"),
    }
}

fn has_paging_params(query: &str) -> bool {
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        !value.is_empty() && matches!(key, "page" | "min_id" | "max_id" | "since_id")
    })
}

/// `\A/api/v\d+/media\z`
fn is_media_path(path: &str) -> bool {
    path.strip_prefix("/api/v")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(version, rest)| {
            !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) && rest == "media"
        })
}

/// `\A/api/v1/statuses/\d+\z`
fn is_status_path(path: &str) -> bool {
    path.strip_prefix("/api/v1/statuses/")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// `\A/api/v1/statuses/\d+/unreblog\z`
fn is_unreblog_path(path: &str) -> bool {
    path.strip_prefix("/api/v1/statuses/")
        .and_then(|rest| rest.strip_suffix("/unreblog"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// The rate-limiting middleware, layered over the whole router.
pub async fn gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // CORS preflights never reach Mastodon's Rack::Attack (rack-cors answers
    // them first); our gate is outermost, so skip them explicitly.
    if request.method() == Method::OPTIONS {
        return next.run(request).await;
    }
    let class = classify(
        request.method(),
        request.uri().path(),
        request.uri().query(),
    );
    if !class.any() {
        return next.run(request).await;
    }
    // The cache serves last-known settings through a failed refresh, so this
    // errors only when no settings row has *ever* loaded (cold start against a
    // down database). Fail open or closed per route — an explicit decision,
    // not an accident of error handling.
    let settings = match state.settings_cache.get(&state.pool).await {
        Ok(settings) => settings,
        Err(err) if class.fails_closed() => {
            tracing::error!(error = %err, "rate limiter could not load instance settings");
            return unavailable_response();
        }
        Err(err) => {
            tracing::error!(error = %err, "rate limiter could not load instance settings");
            return next.run(request).await;
        }
    };
    if !settings.rate_limiting_enabled {
        return next.run(request).await;
    }

    let peer = crate::instance_policy::ip_from_connect_info(
        request.extensions().get::<ConnectInfo<SocketAddr>>(),
    );
    let ip =
        crate::instance_policy::client_ip(request.headers(), peer, &state.config.trusted_proxies)
            .map(throttleable_ip);
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let now = now_epoch();
    let token = match bearer {
        Some(bearer) if class.api || class.paging => {
            // Cheap IP admission *before* the token DB lookup: once an IP has
            // burned its unauthenticated budget (e.g. a flood of bogus bearer
            // strings), stop spending a Postgres lookup resolving each one.
            // Treating the request as unauthenticated lets the same-IP
            // unauthenticated bucket 429 it below without the DB hit. This is a
            // coarse per-IP gate: a legitimate token behind an
            // IP that is already over its unauthenticated budget is caught too,
            // which is the accepted trade-off of pre-resolution admission.
            if ip.is_some_and(|ip| {
                unauth_budget_exhausted(&state.rate_limiter, &class, ip, &settings, now)
            }) {
                None
            } else {
                state.rate_limiter.token_identity(&state.pool, bearer).await
            }
        }
        _ => None,
    };
    let mut tightest: Option<Outcome> = None;
    for (bucket, identity) in matching_buckets(&class, token, ip) {
        let limit = bucket.limit(&settings);
        let outcome = if bucket.durable_key().is_some() {
            match RateLimiter::check_durable(&state.pool, bucket, &identity, limit, now).await {
                Ok(outcome) => outcome,
                // A durable bucket guards a security-sensitive route: fail
                // closed rather than letting a database outage un-meter a
                // brute-force path.
                Err(err) => {
                    tracing::error!(error = %err, ?bucket, "durable rate-limit check failed");
                    return unavailable_response();
                }
            }
        } else {
            state.rate_limiter.check(bucket, identity, limit, now)
        };
        if outcome.throttled {
            return throttled_response(outcome);
        }
        if tightest.is_none_or(|current| outcome.remaining < current.remaining) {
            tightest = Some(outcome);
        }
    }

    let mut response = next.run(request).await;
    if let Some(outcome) = tightest {
        apply_headers(response.headers_mut(), outcome);
    }
    response
}

/// Whether `ip` has already exhausted the unauthenticated budget that a
/// would-be-unauthenticated request of this class draws from — the buckets a
/// request with no resolvable user counts against. When true, resolving the
/// bearer token is pointless work: the request is throttled either way, so the
/// caller skips the DB lookup and lets the normal path 429 it.
fn unauth_budget_exhausted(
    limiter: &RateLimiter,
    class: &RequestClass,
    ip: IpAddr,
    settings: &InstanceSettings,
    now: u64,
) -> bool {
    let over = |bucket: Bucket| {
        limiter.would_throttle(bucket, Identity::Ip(ip), bucket.limit(settings), now)
    };
    (class.api && over(Bucket::UnauthenticatedApi))
        || (class.paging && over(Bucket::UnauthenticatedPaging))
}

/// The (bucket, key) pairs this request counts against.
fn matching_buckets(
    class: &RequestClass,
    token: Option<TokenIdentity>,
    ip: Option<IpAddr>,
) -> Vec<(Bucket, Identity)> {
    let user_id = token.and_then(|token| token.user_id);
    let mut checks = Vec::new();
    if class.api {
        if let Some(user_id) = user_id {
            checks.push((Bucket::AuthenticatedApi, Identity::User(user_id)));
        } else if let Some(ip) = ip {
            // Like Mastodon, a token without a resource owner (an app-level
            // `client_credentials` token) still throttles by IP here.
            checks.push((Bucket::UnauthenticatedApi, Identity::Ip(ip)));
        }
        if let Some(token) = token {
            checks.push((Bucket::PerTokenApi, Identity::Token(token.token_id)));
        }
        if let Some(user_id) = user_id {
            if class.media_post {
                checks.push((Bucket::ApiMedia, Identity::User(user_id)));
            }
            if class.delete_action {
                checks.push((Bucket::ApiDelete, Identity::User(user_id)));
            }
        }
        if let Some(ip) = ip {
            if class.api_sign_up {
                checks.push((Bucket::ApiSignUp, Identity::Ip(ip)));
            }
            if class.app_registration {
                checks.push((Bucket::AppRegistrations, Identity::Ip(ip)));
            }
        }
    }
    if class.paging {
        if let Some(user_id) = user_id {
            checks.push((Bucket::AuthenticatedPaging, Identity::User(user_id)));
        } else if let Some(ip) = ip {
            checks.push((Bucket::UnauthenticatedPaging, Identity::Ip(ip)));
        }
    }
    if let Some(ip) = ip {
        if class.remote_ingress {
            checks.push((Bucket::RemoteIngress, Identity::Ip(ip)));
        }
        if class.login_post {
            checks.push((Bucket::LoginAttemptsIp, Identity::Ip(ip)));
        }
        if class.web_sign_up {
            checks.push((Bucket::SignUpWeb, Identity::Ip(ip)));
        }
        if class.token_endpoint {
            checks.push((Bucket::TokenEndpoint, Identity::Ip(ip)));
        }
    }
    checks
}

/// The e-mail-keyed buckets, called from the handlers that have the form
/// parsed (`POST /login`, `POST /oauth/authorize`, `POST /auth/password`).
/// Returns Mastodon's 429 as an [`ApiError`] once the address is over budget.
/// Both buckets are durable and fail closed: a credential budget must not be
/// waived because the counter store is unreachable.
pub async fn check_email(state: &AppState, bucket: Bucket, email: &str) -> Result<(), ApiError> {
    debug_assert!(matches!(
        bucket,
        Bucket::LoginAttemptsEmail | Bucket::PasswordResetsEmail
    ));
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.rate_limiting_enabled {
        return Ok(());
    }
    let identity = Identity::Email(email.trim().to_lowercase());
    let outcome = RateLimiter::check_durable(
        &state.pool,
        bucket,
        &identity,
        bucket.limit(&settings),
        now_epoch(),
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, ?bucket, "durable rate-limit check failed");
        ApiError::ServiceUnavailable("Rate limiter unavailable".to_owned())
    })?;
    if outcome.throttled {
        Err(ApiError::TooManyRequests)
    } else {
        Ok(())
    }
}

/// The per-account translate-backend budget, called from the translation
/// module just before a backend request (never on a cache hit). `0` disables
/// the limit, as does the global rate-limiting switch. Durable and fail-closed:
/// the bucket meters paid backend spend, which a restart or a counter-store
/// outage must not quietly unmeter.
pub async fn check_translation(state: &AppState, account_id: i64) -> Result<(), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let limit = settings.translation_user_rate_limit_per_hour;
    if !settings.rate_limiting_enabled || limit <= 0 {
        return Ok(());
    }
    let outcome = RateLimiter::check_durable(
        &state.pool,
        Bucket::TranslateBackend,
        &Identity::User(account_id),
        u32::try_from(limit).unwrap_or(u32::MAX),
        now_epoch(),
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "durable rate-limit check failed");
        ApiError::ServiceUnavailable("Rate limiter unavailable".to_owned())
    })?;
    if outcome.throttled {
        Err(ApiError::TooManyRequests)
    } else {
        Ok(())
    }
}

/// The per-account admission budget for expensive first-party web maintenance
/// operations (currently the full-archive request route). Called from the
/// handler, which knows the signed-in account, mirroring [`check_translation`]:
/// the web surface is session- rather than bearer-authenticated, so the
/// middleware [`gate`] cannot key it by account. The 6-day cooldown already
/// bounds how many archives actually build; this bounds the *requests* so spam
/// can't repeatedly take the archive route's advisory lock + transaction before
/// the cooldown refuses it. Disabled with the global switch.
pub async fn check_web_maintenance(state: &AppState, account_id: i64) -> Result<(), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.rate_limiting_enabled {
        return Ok(());
    }
    let outcome = state.rate_limiter.check(
        Bucket::WebMaintenance,
        Identity::User(account_id),
        Bucket::WebMaintenance.limit(&settings),
        now_epoch(),
    );
    if outcome.throttled {
        Err(ApiError::TooManyRequests)
    } else {
        Ok(())
    }
}

/// The per-account and per-IP admission budget for CSV import uploads, called
/// from the handler *before* it buffers the multipart body. Mirrors
/// [`check_web_maintenance`]: the web upload surface is session- rather than
/// bearer-authenticated, so the middleware [`gate`] cannot key it by account,
/// and the check must run pre-buffer so a burst of simultaneous uploads is
/// turned away before each one reads ~21 MiB into memory. The IP
/// leg (masked to a network prefix like every other IP bucket) catches one
/// address cycling through many accounts. Disabled with the global switch.
pub async fn check_import_upload(
    state: &AppState,
    account_id: i64,
    ip: Option<IpAddr>,
) -> Result<(), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.rate_limiting_enabled {
        return Ok(());
    }
    let limit = Bucket::ImportUpload.limit(&settings);
    let now = now_epoch();
    let account_throttled = state
        .rate_limiter
        .check(Bucket::ImportUpload, Identity::User(account_id), limit, now)
        .throttled;
    let ip_throttled = ip.is_some_and(|ip| {
        state
            .rate_limiter
            .check(
                Bucket::ImportUpload,
                Identity::Ip(throttleable_ip(ip)),
                limit,
                now,
            )
            .throttled
    });
    if account_throttled || ip_throttled {
        Err(ApiError::TooManyRequests)
    } else {
        Ok(())
    }
}

/// The per-account and per-IP admission budget for local group creation, called
/// from the handler *before* it generates the group's RSA key. Mirrors
/// [`check_import_upload`]: the web surface is session- rather than
/// bearer-authenticated, so the middleware [`gate`] cannot key it by account,
/// and the check must run before the expensive key generation so a burst of
/// concurrent creates is turned away up front. The per-account
/// quota (`groups::MAX_GROUPS_PER_ACCOUNT`) bounds the total; this bounds the
/// rate. Disabled with the global switch.
pub async fn check_group_create(
    state: &AppState,
    account_id: i64,
    ip: Option<IpAddr>,
) -> Result<(), ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.rate_limiting_enabled {
        return Ok(());
    }
    let limit = Bucket::GroupCreate.limit(&settings);
    let now = now_epoch();
    let account_throttled = state
        .rate_limiter
        .check(Bucket::GroupCreate, Identity::User(account_id), limit, now)
        .throttled;
    let ip_throttled = ip.is_some_and(|ip| {
        state
            .rate_limiter
            .check(
                Bucket::GroupCreate,
                Identity::Ip(throttleable_ip(ip)),
                limit,
                now,
            )
            .throttled
    });
    if account_throttled || ip_throttled {
        Err(ApiError::TooManyRequests)
    } else {
        Ok(())
    }
}

fn apply_headers(headers: &mut HeaderMap, outcome: Outcome) {
    let put = |headers: &mut HeaderMap, name: &'static str, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    };
    put(headers, "x-ratelimit-limit", outcome.limit.to_string());
    put(
        headers,
        "x-ratelimit-remaining",
        outcome.remaining.to_string(),
    );
    put(
        headers,
        "x-ratelimit-reset",
        reset_iso8601(outcome.reset_epoch),
    );
}

/// The fail-closed refusal: a security-sensitive admission check could not run
/// (settings never loaded, or the durable counter store is unreachable), so
/// the request is refused rather than admitted unmetered. `503` — the outage
/// is ours, not the client's.
fn unavailable_response() -> Response {
    ApiError::ServiceUnavailable("Rate limiter unavailable".to_owned()).into_response()
}

fn throttled_response(outcome: Outcome) -> Response {
    let mut response = ApiError::TooManyRequests.into_response();
    apply_headers(
        response.headers_mut(),
        Outcome {
            remaining: 0,
            ..outcome
        },
    );
    // The rate-limit gate sits outside the per-group CORS layers, so a
    // browser client could not read the 429 without these.
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("x-ratelimit-limit, x-ratelimit-remaining, x-ratelimit-reset"),
    );
    response
}

/// Ruby's `Time#iso8601(6)` for a whole-second UTC instant — what Mastodon
/// puts in `X-RateLimit-Reset`.
fn reset_iso8601(epoch: u64) -> String {
    let timestamp =
        time::OffsetDateTime::from_unix_timestamp(i64::try_from(epoch).unwrap_or(i64::MAX))
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000000Z",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_window_counts_and_resets() {
        let limiter = RateLimiter::default();
        let identity = Identity::User(7);
        let first = limiter.check(Bucket::PerTokenApi, identity.clone(), 2, 1000);
        assert!(!first.throttled);
        assert_eq!(first.remaining, 1);
        assert_eq!(first.reset_epoch, 1200);
        let second = limiter.check(Bucket::PerTokenApi, identity.clone(), 2, 1001);
        assert!(!second.throttled);
        assert_eq!(second.remaining, 0);
        let third = limiter.check(Bucket::PerTokenApi, identity.clone(), 2, 1002);
        assert!(third.throttled);
        // Next window: the counter starts over.
        let fresh = limiter.check(Bucket::PerTokenApi, identity, 2, 1200);
        assert!(!fresh.throttled);
        assert_eq!(fresh.remaining, 1);
    }

    #[test]
    fn buckets_and_identities_are_independent() {
        let limiter = RateLimiter::default();
        let over = limiter.check(Bucket::ApiSignUp, Identity::User(1), 1, 50);
        assert!(!over.throttled);
        assert!(
            limiter
                .check(Bucket::ApiSignUp, Identity::User(1), 1, 51)
                .throttled
        );
        assert!(
            !limiter
                .check(Bucket::ApiSignUp, Identity::User(2), 1, 52)
                .throttled
        );
        assert!(
            !limiter
                .check(Bucket::ApiDelete, Identity::User(1), 1, 53)
                .throttled
        );
    }

    #[test]
    fn web_maintenance_budget_is_per_account_and_bounded() {
        let limiter = RateLimiter::default();
        let now = 5_000;
        let check = |account: i64| {
            limiter
                .check(
                    Bucket::WebMaintenance,
                    Identity::User(account),
                    WEB_MAINTENANCE_BUDGET,
                    now,
                )
                .throttled
        };
        // The whole budget is admitted for one account…
        for _ in 0..WEB_MAINTENANCE_BUDGET {
            assert!(!check(1));
        }
        // …then it is throttled — spam past the budget is refused before the
        // handler ever reaches the archive route's advisory lock/DB (finding #44).
        assert!(check(1));
        // A different account keeps its own budget.
        assert!(!check(2));
    }

    #[test]
    fn import_upload_budget_is_per_account_and_per_ip() {
        let limiter = RateLimiter::default();
        let now = 9_000;
        let by = |identity: Identity| {
            limiter
                .check(Bucket::ImportUpload, identity, IMPORT_UPLOAD_BUDGET, now)
                .throttled
        };
        // One account burns its whole budget, then is refused — pre-buffer, so
        // an upload flood is turned away before it reads ~21 MiB (finding #51).
        for _ in 0..IMPORT_UPLOAD_BUDGET {
            assert!(!by(Identity::User(1)));
        }
        assert!(by(Identity::User(1)));
        // A second account keeps its own account budget…
        assert!(!by(Identity::User(2)));
        // …but one IP cycling accounts shares an IP budget, so it is bounded too.
        let ip = Identity::Ip("203.0.113.4".parse().unwrap());
        for _ in 0..IMPORT_UPLOAD_BUDGET {
            assert!(!by(ip.clone()));
        }
        assert!(by(ip));
    }

    #[test]
    fn group_create_budget_is_per_account_and_per_ip() {
        let limiter = RateLimiter::default();
        let now = 11_000;
        let by = |identity: Identity| {
            limiter
                .check(Bucket::GroupCreate, identity, GROUP_CREATE_BUDGET, now)
                .throttled
        };
        // One account burns its whole budget, then is refused — before any RSA
        // key is generated (finding #61).
        for _ in 0..GROUP_CREATE_BUDGET {
            assert!(!by(Identity::User(1)));
        }
        assert!(by(Identity::User(1)));
        // A second account keeps its own budget…
        assert!(!by(Identity::User(2)));
        // …while one IP cycling accounts is bounded by a shared IP budget.
        let ip = Identity::Ip("203.0.113.7".parse().unwrap());
        for _ in 0..GROUP_CREATE_BUDGET {
            assert!(!by(ip.clone()));
        }
        assert!(by(ip));
    }

    #[test]
    fn would_throttle_peeks_without_consuming_budget() {
        let limiter = RateLimiter::default();
        let ip = Identity::Ip("198.51.100.9".parse().unwrap());
        let now = 2_000;
        let limit = 2;
        let bucket = Bucket::UnauthenticatedApi;
        // A read-only peek never consumes budget: repeated peeks on an empty
        // window stay false no matter how many times they run.
        assert!(!limiter.would_throttle(bucket, ip.clone(), limit, now));
        assert!(!limiter.would_throttle(bucket, ip.clone(), limit, now));
        // Spend the whole budget with real (incrementing) checks.
        assert!(!limiter.check(bucket, ip.clone(), limit, now).throttled);
        assert!(!limiter.check(bucket, ip.clone(), limit, now).throttled);
        // Now a further request *would* throttle — the signal that lets `gate`
        // skip the token DB lookup so a flooded IP stops reaching Postgres
        // (finding #52).
        assert!(limiter.would_throttle(bucket, ip.clone(), limit, now));
        // A fresh window clears it.
        assert!(!limiter.would_throttle(bucket, ip, limit, now + bucket.period_secs()));
    }

    #[tokio::test]
    async fn concurrent_identical_token_lookups_coalesce_to_one_resolution() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let limiter = Arc::new(RateLimiter::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let identity = TokenIdentity {
            token_id: 1,
            user_id: Some(2),
        };

        let spawn_lookup = |hash: &str| {
            let limiter = Arc::clone(&limiter);
            let calls = Arc::clone(&calls);
            let hash = hash.to_owned();
            tokio::spawn(async move {
                limiter
                    .resolve_coalesced(hash, || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Hold the winner long enough for the others to queue
                        // behind the single-flight lock.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(Some(identity))
                    })
                    .await
            })
        };

        // Sixteen concurrent lookups of the *same* token resolve exactly once…
        let same: Vec<_> = (0..16).map(|_| spawn_lookup("same-token-hash")).collect();
        // …while a different token is resolved on its own.
        let other = spawn_lookup("other-token-hash");
        for handle in same {
            assert_eq!(handle.await.unwrap(), Some(identity));
        }
        assert_eq!(other.await.unwrap(), Some(identity));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "identical lookups must coalesce to one resolution; a distinct token adds one more",
        );
    }

    #[test]
    fn ipv6_throttles_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(throttleable_ip(a), throttleable_ip(b));
        assert_ne!(throttleable_ip(a), throttleable_ip(c));
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(throttleable_ip(v4), v4);
    }

    #[test]
    fn classification_matches_mastodon_throttle_paths() {
        let post = Method::POST;
        let get = Method::GET;
        assert!(classify(&post, "/api/v2/media", None).media_post);
        assert!(!classify(&post, "/api/v1/media/5", None).media_post);
        assert!(classify(&Method::DELETE, "/api/v1/statuses/123", None).delete_action);
        assert!(classify(&post, "/api/v1/statuses/123/unreblog", None).delete_action);
        assert!(!classify(&post, "/api/v1/statuses/123/reblog", None).delete_action);
        assert!(classify(&post, "/api/v1/apps", None).app_registration);
        assert!(classify(&post, "/api/v1/accounts", None).api_sign_up);
        assert!(classify(&post, "/login", None).login_post);
        assert!(classify(&post, "/oauth/authorize", None).login_post);
        // The WebAuthn ceremonies and confirmation resend share the
        // credential admission ceiling.
        assert!(classify(&post, "/login/webauthn/options", None).login_post);
        assert!(classify(&post, "/login/webauthn", None).login_post);
        assert!(classify(&post, "/oauth/authorize/webauthn/options", None).login_post);
        assert!(classify(&post, "/oauth/authorize/webauthn", None).login_post);
        assert!(classify(&post, "/api/v1/emails/confirmations", None).login_post);
        assert!(!classify(&get, "/login/webauthn", None).login_post);
        assert!(classify(&post, "/signup", None).web_sign_up);
        assert!(classify(&get, "/api/v1/timelines/home", Some("max_id=5")).paging);
        assert!(!classify(&get, "/api/v1/timelines/home", Some("max_id=")).paging);
        assert!(!classify(&get, "/api/v1/timelines/home", Some("limit=5")).paging);
        assert!(classify(&get, "/users/alice", None).remote_ingress);
        assert!(classify(&post, "/inbox", None).remote_ingress);
        assert!(classify(&get, "/media/hls/1/master.m3u8", None).remote_ingress);
        // OAuth token/revoke get a per-IP admission bucket.
        assert!(classify(&post, "/oauth/token", None).token_endpoint);
        assert!(classify(&post, "/oauth/revoke", None).token_endpoint);
        assert!(!classify(&get, "/oauth/token", None).token_endpoint);
        assert_eq!(
            matching_buckets(
                &classify(&post, "/oauth/token", None),
                None,
                Some("192.0.2.1".parse().unwrap()),
            ),
            vec![(
                Bucket::TokenEndpoint,
                Identity::Ip("192.0.2.1".parse().unwrap())
            )],
        );
    }

    #[test]
    fn reset_header_is_ruby_iso8601_micros() {
        assert_eq!(reset_iso8601(1_750_000_200), "2025-06-15T15:10:00.000000Z");
    }

    /// The explicit fail-open/closed register: the classes that
    /// guard credentials, account/app creation and token minting refuse
    /// service when the limiter cannot run; everything else is load shedding
    /// and stays available.
    #[test]
    fn credential_classes_fail_closed_and_load_shedding_fails_open() {
        let post = Method::POST;
        let get = Method::GET;
        for (method, path) in [
            (&post, "/login"),
            (&post, "/signup"),
            (&post, "/oauth/token"),
            (&post, "/api/v1/accounts"),
            (&post, "/api/v1/apps"),
        ] {
            assert!(
                classify(method, path, None).fails_closed(),
                "{path} must fail closed"
            );
        }
        for (method, path) in [
            (&get, "/api/v1/timelines/home"),
            (&get, "/users/alice"),
            (&post, "/inbox"),
        ] {
            assert!(
                !classify(method, path, None).fails_closed(),
                "{path} must fail open"
            );
        }
    }

    /// Durable buckets must not collide in storage, and e-mail identities must
    /// be hashed before they reach the shared table (no third-party PII in
    /// `rate_limit_windows`).
    #[test]
    fn durable_keys_are_distinct_and_emails_are_hashed() {
        let durable = [
            Bucket::LoginAttemptsIp,
            Bucket::LoginAttemptsEmail,
            Bucket::PasswordResetsEmail,
            Bucket::ApiSignUp,
            Bucket::SignUpWeb,
            Bucket::AppRegistrations,
            Bucket::TokenEndpoint,
            Bucket::TranslateBackend,
        ];
        let keys: std::collections::HashSet<_> = durable
            .iter()
            .map(|bucket| bucket.durable_key().expect("durable bucket has a key"))
            .collect();
        assert_eq!(keys.len(), durable.len(), "storage keys must be distinct");

        let identity = Identity::Email("victim@example.com".to_owned());
        let key = identity.durable_key();
        assert!(key.starts_with("email:"));
        assert!(
            !key.contains("victim") && !key.contains("example.com"),
            "raw address must not reach storage: {key}"
        );

        // The hot-path buckets deliberately stay in-process.
        assert!(Bucket::AuthenticatedApi.durable_key().is_none());
        assert!(Bucket::RemoteIngress.durable_key().is_none());
    }
}
