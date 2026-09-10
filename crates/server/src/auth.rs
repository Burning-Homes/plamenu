//! Credentials, tokens, and the request authentication extractor.
//!
//! Secrets at rest: passwords are argon2id PHC strings; access tokens,
//! authorization codes and client secrets are random 256-bit values of which
//! only the SHA-256 is stored (deterministic, so lookups stay indexable).

use std::net::SocketAddr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use plamenu_db::account::{self, Account};
use plamenu_db::oauth;
use plamenu_db::role::{self, Role};
use plamenu_db::user::{self, User};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::error::ApiError;

/// Number of Argon2 password *hashes* computed this process. Incremented by
/// [`hash_password`]. Exposed via [`password_hash_count`] so the reset-completion
/// "validate the token before spending a hash" ordering can be
/// asserted deterministically, and as a cheap operational signal.
static PASSWORD_HASH_COUNT: AtomicU64 = AtomicU64::new(0);

/// Argon2 password hashes computed since process start.
#[must_use]
pub fn password_hash_count() -> u64 {
    PASSWORD_HASH_COUNT.load(Ordering::Relaxed)
}

/// Hashes a password with argon2id (default = current OWASP parameters).
pub fn hash_password(password: &str) -> Result<String, ApiError> {
    PASSWORD_HASH_COUNT.fetch_add(1, Ordering::Relaxed);
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| ApiError::Internal(e.to_string().into()))
}

/// Number of genuine Argon2 verifications performed this process. Incremented
/// by [`verify_password`] each time it actually runs the hash (i.e. when it was
/// given a well-formed PHC string), and therefore also by
/// [`verify_password_dummy`]. Exposed via [`password_verify_count`] so the
/// login-timing equalization can be asserted deterministically,
/// and as a cheap operational signal.
static PASSWORD_VERIFY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Genuine Argon2 password verifications performed since process start.
#[must_use]
pub fn password_verify_count() -> u64 {
    PASSWORD_VERIFY_COUNT.load(Ordering::Relaxed)
}

#[must_use]
pub fn verify_password(password: &str, password_hash: &str) -> bool {
    PasswordHash::new(password_hash).is_ok_and(|parsed| {
        PASSWORD_VERIFY_COUNT.fetch_add(1, Ordering::Relaxed);
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// [`hash_password`] on Tokio's blocking pool behind the credential-crypto
/// concurrency gate ([`crate::crypto_gate`]). Argon2id is deliberately CPU- and
/// memory-heavy, so running it inline on an async worker thread lets a burst of
/// accepted registrations/resets stall unrelated request handling and the
/// background workers sharing the runtime; this keeps that work
/// off the async threads and bounded in concurrency. Every async request path
/// that hashes a password uses this rather than [`hash_password`] directly; the
/// synchronous version stays for one-shot CLI commands and tests.
pub async fn hash_password_gated(password: String) -> Result<String, ApiError> {
    let _permit = crate::crypto_gate::acquire().await;
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
}

/// [`verify_password`] on the blocking pool behind the credential-crypto gate
/// A panic in the blocking task fails closed (`false`), like a
/// wrong password — a verification never authenticates on a crypto fault.
pub async fn verify_password_gated(password: String, password_hash: String) -> bool {
    let _permit = crate::crypto_gate::acquire().await;
    tokio::task::spawn_blocking(move || verify_password(&password, &password_hash))
        .await
        .unwrap_or(false)
}

/// An id no `users` row can ever have (snowflake ids are positive), used by
/// [`equalized_failed_login`]'s no-effect database statements.
const NO_SUCH_USER: i64 = -1;

/// The full work-equalized failure path for a login whose identifier matched
/// no account: performs the same *classes* of database round
/// trips a wrong password for a real account costs — one indexed `users` read
/// before the hash check, two update statements after it — against an id no
/// row can have, with the dummy Argon2 verification in between exactly where
/// the real hash check sits. The generic error message alone is not enough:
/// without this, an unknown identifier answered after one read while a known
/// one cost a read, an expensive hash, and two writes, a remotely measurable
/// difference. Shared by the web and OAuth password surfaces.
pub async fn equalized_failed_login(state: &crate::state::AppState, password: String) {
    let _ = plamenu_db::user::login_locked(&state.pool, NO_SUCH_USER).await;
    verify_password_dummy_gated(password).await;
    // The real wrong-password branch records a login-activity row and the
    // failed-sign-in counter: two write round trips. These match the count and
    // class (single-row statements) while touching nothing.
    let _ = plamenu_db::user::record_failed_sign_in(&state.pool, NO_SUCH_USER).await;
    let _ = plamenu_db::user::record_failed_sign_in(&state.pool, NO_SUCH_USER).await;
}

/// [`verify_password_dummy`] on the blocking pool behind the credential-crypto
/// gate — the no-such-account branch's timing-equalizing work, kept off the
/// async threads and bounded like the real verify.
pub async fn verify_password_dummy_gated(password: String) {
    let _permit = crate::crypto_gate::acquire().await;
    let _ = tokio::task::spawn_blocking(move || verify_password_dummy(&password)).await;
}

/// Generates a local actor RSA-2048 keypair on Tokio's blocking pool behind the
/// credential-crypto gate. RSA keygen is CPU-bound; running it inline on an
/// async worker thread (registration, group creation) let a burst monopolize
/// the runtime. The `KeyError` is surfaced as an internal error,
/// matching the previous inline call sites.
pub async fn generate_keypair_gated() -> Result<plamenu_ap::keys::KeyPairPem, ApiError> {
    let _permit = crate::crypto_gate::acquire().await;
    tokio::task::spawn_blocking(plamenu_ap::keys::generate_keypair)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
        .map_err(|e| ApiError::Internal(e.to_string().into()))
}

/// A valid argon2id hash of a random, never-matched password, computed once with
/// the current default parameters. Because the parameters live inside the PHC
/// string, verifying against it costs exactly what verifying a real stored hash
/// costs — the point of the dummy verification below.
static DUMMY_PASSWORD_HASH: LazyLock<String> = LazyLock::new(|| {
    hash_password(&generate_secret()).expect("hashing a random dummy password cannot fail")
});

/// Spends one Argon2 verification against a fixed dummy hash and discards the
/// (always `false`) result.
///
/// Call this on the *no such account* branch of a password login so that an
/// attempt for a non-existent identifier performs the same expensive Argon2
/// work as one for a real account with a wrong password. Otherwise the missing
/// branch returns after only a cheap indexed lookup, giving a remote timing
/// oracle for account (e-mail) enumeration despite the generic error message.
pub fn verify_password_dummy(password: &str) {
    // The result is meaningless — no password matches the random dummy — so it
    // is discarded; only the constant-time-ish Argon2 work matters here.
    let _ = verify_password(password, &DUMMY_PASSWORD_HASH);
}

/// The password-length policy shared by *every* surface that sets a local
/// account's password — registration, self-service reset, the signed-in change
/// form, and the operator CLI. Devise's 8-character floor and the bcrypt-era
/// 72-character ceiling Mastodon still keeps. Centralized here so
/// no individual form can quietly weaken it.
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 72;

/// Why a candidate password fails [`validate_password`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordPolicy {
    /// No password was supplied.
    Empty,
    /// Fewer than [`PASSWORD_MIN`] characters.
    TooShort,
    /// More than [`PASSWORD_MAX`] characters.
    TooLong,
}

impl PasswordPolicy {
    /// A human-readable, surface-agnostic reason. The wording matches what the
    /// reset and signed-in change forms already showed, so centralizing on it
    /// changes no user-visible copy.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Empty => "Choose a non-empty new password.".to_owned(),
            Self::TooShort => {
                format!("Password is too short (minimum is {PASSWORD_MIN} characters).")
            }
            Self::TooLong => {
                format!("Password is too long (maximum is {PASSWORD_MAX} characters).")
            }
        }
    }
}

/// Validates a plaintext password against the shared length policy; `Ok(())`
/// means it may be hashed. Counts Unicode scalar values, matching the
/// registration and reset forms (and, therefore, the length the user sees).
///
/// # Errors
///
/// Returns the specific [`PasswordPolicy`] violation so each caller can render
/// it in its own idiom (Mastodon field codes, a flash message, a CLI error).
pub fn validate_password(password: &str) -> Result<(), PasswordPolicy> {
    let length = password.chars().count();
    if length == 0 {
        Err(PasswordPolicy::Empty)
    } else if length < PASSWORD_MIN {
        Err(PasswordPolicy::TooShort)
    } else if length > PASSWORD_MAX {
        Err(PasswordPolicy::TooLong)
    } else {
        Ok(())
    }
}

/// A fresh 256-bit secret (token, code, client id/secret), base64url.
#[must_use]
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS entropy source failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The storage form of a secret.
#[must_use]
pub fn hash_secret(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

/// Constant-time equality for fixed-length stored secret digests.
#[must_use]
pub fn secret_hash_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

/// The primary language subtag of an `Accept-Language` header (e.g. `en` from
/// `en-US,en;q=0.9`), used to seed `users.locale` at first sign-in for the admin
/// `languages` dimension. `None` when the header is absent or a wildcard.
#[must_use]
pub fn accept_language_primary(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)?
        .to_str()
        .ok()?;
    let first = raw.split(',').next()?.split(';').next()?.trim();
    let base = first.split('-').next()?.trim().to_ascii_lowercase();
    (!base.is_empty() && base != "*").then_some(base)
}

/// The `User-Agent` header as an owned string, trimmed to a sane length so a
/// hostile client can't bloat the session/login-activity rows. `None` when
/// absent or not valid UTF-8.
#[must_use]
pub fn user_agent_string(headers: &axum::http::HeaderMap) -> Option<String> {
    const MAX_UA_LEN: usize = 500;
    let raw = headers
        .get(axum::http::header::USER_AGENT)?
        .to_str()
        .ok()?
        .trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.chars().take(MAX_UA_LEN).collect())
}

/// The site's canonical browser origin — `https://{domain}`. Every generated
/// URL and every `Secure` cookie already assumes HTTPS on the bare domain
/// (`config::validate_domain` forbids a scheme, port, or path), so this is the
/// one origin a first-party form submission can legitimately come from.
#[must_use]
pub fn site_origin(domain: &str) -> String {
    format!("https://{domain}")
}

/// Whether a browser request demonstrably originated from our own site.
///
/// Signed-in mutations are guarded by a session-derived CSRF token, but the
/// sign-in transition itself has no session to key a token to, which leaves the
/// pre-session credential POSTs (`POST /login`, `/login/challenge`, and the web
/// OAuth consent `POST /oauth/authorize`) open to login CSRF / session
/// swapping: a cross-site form auto-submitting attacker credentials would sign
/// the victim's browser into the *attacker's* account.
/// `SameSite=Lax` stops an *existing* session cookie from riding along but is
/// not proof the browser initiated this new login, so we additionally require
/// the browser's own `Origin` (or, absent that, `Referer`) to match
/// [`site_origin`]:
///
/// - `Origin` present → must equal our origin exactly. Modern browsers attach
///   `Origin` to every cross-site (and same-site) POST, so the attack's form
///   carries `Origin: https://attacker.example` and is refused; an opaque
///   `Origin: null` (sandboxed iframe / `data:` document) never matches either.
/// - `Origin` absent but `Referer` present → the referrer's origin must match.
/// - both absent → allow. This is the compatible fallback: a browser mounting
///   the attack always sends `Origin`, and a non-browser client carries no
///   ambient cookies, so it has no session to swap and nothing to protect. It
///   keeps legitimate programmatic posters and header-stripping privacy tools
///   working.
#[must_use]
pub fn same_origin_request(headers: &axum::http::HeaderMap, domain: &str) -> bool {
    let expected = site_origin(domain);
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        return origin.eq_ignore_ascii_case(&expected);
    }
    if let Some(referer) = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
    {
        return referer_origin_matches(referer, &expected);
    }
    true
}

/// Whether the origin (scheme + host, no path/query/fragment) of a full
/// `Referer` URL equals `expected_origin` (`https://host`, no trailing slash).
/// A missing scheme, or any host mismatch — including a userinfo prefix
/// (`https://plamenu.test@evil.example/`) or a lookalike subdomain
/// (`https://plamenu.test.evil.example/`) — fails the match.
fn referer_origin_matches(referer: &str, expected_origin: &str) -> bool {
    let Some((scheme, rest)) = referer.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    format!("{scheme}://{host}").eq_ignore_ascii_case(expected_origin)
}

/// PKCE S256: the challenge for a verifier.
#[must_use]
pub fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Resolves a sign-in identifier to its local user: one field accepts the
/// e-mail address or the username interchangeably. A leading `@` is
/// tolerated, and `name@<this server's domain>` counts as the local handle.
/// An identifier with an `@` inside is tried as an e-mail address first, so
/// users whose mailbox lives on the instance domain can still sign in with it.
pub async fn find_user_by_login_identifier(
    state: &AppState,
    identifier: &str,
) -> Result<Option<User>, ApiError> {
    let identifier = identifier.trim();
    let identifier = identifier.strip_prefix('@').unwrap_or(identifier);
    if identifier.is_empty() {
        return Ok(None);
    }
    if let Some((local_part, domain)) = identifier.rsplit_once('@') {
        if let Some(user) = user::find_by_email(&state.pool, identifier).await? {
            return Ok(Some(user));
        }
        if state.config.is_local_domain(domain) {
            return find_user_by_local_username(state, local_part).await;
        }
        return Ok(None);
    }
    find_user_by_local_username(state, identifier).await
}

/// A password sign-in failure after applying the same account-state and
/// timing-equalisation policy on every credential surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordLoginError {
    BadCredentials,
    Locked,
    Unconfirmed,
    PendingApproval,
    Disabled,
    EmailBlocked,
}

impl PasswordLoginError {
    /// Human-facing wording used by the first-party web login. API adapters
    /// translate the same variants into their protocol's stable error codes.
    #[must_use]
    pub const fn web_message(self) -> &'static str {
        match self {
            Self::BadCredentials => "Wrong e-mail/username or password.",
            Self::Locked => "Your account is temporarily locked. Try again later.",
            Self::Unconfirmed => "You have to confirm your e-mail address before signing in.",
            Self::PendingApproval => "Your account is still awaiting approval by the moderators.",
            Self::Disabled => "Your account is currently disabled.",
            Self::EmailBlocked => "This e-mail address is blocked.",
        }
    }
}

/// Verify a username/e-mail and password with Plamenu's complete sign-in
/// policy. This is shared by the HTML login and protocol compatibility
/// adapters so no alternative client API can accidentally skip timing
/// equalisation, lockout accounting, login-history failures, or account-state
/// checks.
pub async fn authenticate_password(
    state: &AppState,
    identifier: &str,
    password: String,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<User, PasswordLoginError> {
    let user = find_user_by_login_identifier(state, identifier)
        .await
        .map_err(|_| PasswordLoginError::BadCredentials)?;
    let Some(user) = user else {
        equalized_failed_login(state, password).await;
        return Err(PasswordLoginError::BadCredentials);
    };
    if user::login_locked(&state.pool, user.id)
        .await
        .map_err(|_| PasswordLoginError::BadCredentials)?
    {
        return Err(PasswordLoginError::Locked);
    }
    if !verify_password_gated(password, user.password_hash.clone()).await {
        let _ = plamenu_db::login_activity::record_failure(
            &state.pool,
            user.id,
            "password",
            ip,
            user_agent,
            "invalid_password",
        )
        .await;
        let locked = user::record_failed_sign_in(&state.pool, user.id)
            .await
            .map_err(|_| PasswordLoginError::BadCredentials)?;
        return Err(if locked {
            PasswordLoginError::Locked
        } else {
            PasswordLoginError::BadCredentials
        });
    }
    let email_blocked = match user.email.as_deref() {
        Some(email) => crate::instance_policy::ensure_email_login_allowed(&state.pool, email)
            .await
            .is_err(),
        None => false,
    };
    if !user.confirmed() {
        Err(PasswordLoginError::Unconfirmed)
    } else if !user.approved {
        Err(PasswordLoginError::PendingApproval)
    } else if user.disabled {
        Err(PasswordLoginError::Disabled)
    } else if email_blocked {
        Err(PasswordLoginError::EmailBlocked)
    } else {
        Ok(user)
    }
}

async fn find_user_by_local_username(
    state: &AppState,
    username: &str,
) -> Result<Option<User>, ApiError> {
    let Some(account) = account::find_local_by_username(&state.pool, username).await? else {
        return Ok(None);
    };
    Ok(user::find_by_account_id(&state.pool, account.id).await?)
}

/// The authenticated user behind a Bearer token. Rejects app-level
/// (`client_credentials`) tokens: those have no user.
pub struct CurrentUser {
    pub user: User,
    pub account: Account,
    pub scopes: String,
    /// OAuth application that minted this bearer token.
    pub app_id: i64,
    /// Row id of the bearer token behind this request — push subscriptions
    /// are keyed to the token, not the user.
    pub token_id: i64,
}

/// Whether a whitespace-separated set of granted OAuth `scopes` satisfies a
/// `required` scope, using the hierarchy Mastodon enforces: a **broad** family
/// grant covers the resource-specific requirements beneath it (`read` covers
/// `read:statuses`, `admin:read` covers `admin:read:reports`), but a
/// resource-specific grant must **never** widen back into its family (a
/// `read:statuses` token satisfies neither a bare `read` requirement nor a
/// sibling `read:notifications`). Only the `:`-delimited segment boundary
/// counts, so `read` does not spuriously cover `readable`.
///
/// This is the single source of truth for the scope lattice — every
/// `require_scope`/`has_scope` path routes through it, so the direction cannot
/// drift between the user-token and app-token surfaces.
#[must_use]
pub fn scopes_satisfy(scopes: &str, required: &str) -> bool {
    scopes
        .split_whitespace()
        .any(|granted| scope_covers(granted, required))
}

/// Whether a single `granted` scope covers `required`: an exact match, or
/// `granted` is a strict `:`-delimited parent of `required` (the broad-covers-
/// narrow direction only).
fn scope_covers(granted: &str, required: &str) -> bool {
    granted == required
        || required
            .strip_prefix(granted)
            .is_some_and(|rest| rest.starts_with(':'))
}

impl CurrentUser {
    /// Whether the token's scopes satisfy `required`, honouring the scope
    /// lattice ([`scopes_satisfy`]): a broad family grant covers the granular
    /// requirements beneath it, but a granular grant never widens into its
    /// family.
    #[must_use]
    pub fn has_scope(&self, required: &str) -> bool {
        scopes_satisfy(&self.scopes, required)
    }

    #[must_use]
    pub fn has_exact_scope(&self, scope: &str) -> bool {
        self.scopes
            .split_whitespace()
            .any(|granted| granted == scope)
    }

    /// A `403` unless the token's scopes satisfy `required`. A broad family
    /// grant (`write`) covers the granular requirements beneath it
    /// (`write:statuses`); a granular grant (`write:statuses`) does **not**
    /// satisfy a broad `write` requirement — the hierarchy Mastodon enforces.
    pub fn require_scope(&self, required: &str) -> Result<(), ApiError> {
        if self.has_scope(required) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "This action is outside the authorized scopes".into(),
            ))
        }
    }
}

/// An authenticated user whose account carries a moderation role — the
/// extractor behind every `/api/v1|v2/admin/*` endpoint. The specific
/// permission and the read/write scope are asserted per-handler via
/// [`AdminUser::require`], mirroring Mastodon's `authorize_with_role` +
/// doorkeeper scope check.
pub struct AdminUser {
    pub current: CurrentUser,
    pub role: Role,
}

impl AdminUser {
    /// Require a permission bit (from `plamenu_db::role::permission`) *and* the
    /// matching admin scope (`admin:read` for reads, `admin:write` for writes).
    /// A `403` if either is missing.
    pub fn require(&self, permission: i64, write: bool) -> Result<(), ApiError> {
        self.current
            .require_scope(if write { "admin:write" } else { "admin:read" })?;
        if self.role.can(permission) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "This action requires a moderator or administrator role".into(),
            ))
        }
    }

    /// Require a permission bit and either the broad admin scope
    /// (`admin:read`/`admin:write`) or the matching resource-specific scope
    /// (`admin:read:reports`, `admin:write:domain_blocks`, ...). This keeps
    /// narrow admin scopes from spilling across unrelated admin resources.
    pub fn require_resource(
        &self,
        permission: i64,
        write: bool,
        resource: &str,
    ) -> Result<(), ApiError> {
        let base = if write { "admin:write" } else { "admin:read" };
        let scoped = format!("{base}:{resource}");
        if !(self.current.has_exact_scope(base) || self.current.has_scope(&scoped)) {
            return Err(ApiError::Forbidden(
                "This action is outside the authorized scopes".into(),
            ));
        }
        if self.role.can(permission) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "This action requires a moderator or administrator role".into(),
            ))
        }
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let current = CurrentUser::from_request_parts(parts, state).await?;
        // The default "User" role grants only the everyone baseline —
        // holding it must not open the admin surfaces, so the gate is
        // "privileged role", not "any role".
        let role = role::for_user(&state.pool, current.user.id)
            .await?
            .filter(Role::privileged)
            .ok_or_else(|| {
                ApiError::Forbidden("This action requires a moderator or administrator role".into())
            })?;
        Ok(Self { current, role })
    }
}

pub const INVALID_TOKEN: &str = "The access token is invalid";

/// Hard server-side lifetime of a first-party web session. The session cookie
/// advertises a 90-day `Max-Age`, but the backing OAuth token used to have no
/// server-side expiry — so a copied/leaked session token stayed valid forever
/// and a roster-evicted one lingered as a live credential the browser no longer
/// tracked. A web-app token past this age is now rejected exactly
/// like a revoked one, making the advertised lifetime real on both ends. Third-
/// party API tokens are unaffected: they keep Mastodon's non-expiring semantics
/// (Phanpy/Elk hold long-lived tokens by design).
pub const MAX_WEB_SESSION_AGE: time::Duration = time::Duration::days(90);

/// Optional authentication: `None` when no `Authorization` header is present,
/// but a *present-and-invalid* token is still a 401 (never silently ignored).
pub struct MaybeUser(pub Option<CurrentUser>);

impl MaybeUser {
    /// Gate an anonymously-readable endpoint behind a preview flag: a signed-in
    /// viewer always passes, an anonymous one only when `enabled`. Otherwise a
    /// 401, matching Mastodon's `DISALLOW_UNAUTHENTICATED_API_ACCESS`.
    pub fn require_preview(self, enabled: bool) -> Result<Option<CurrentUser>, ApiError> {
        match self.0 {
            Some(user) => Ok(Some(user)),
            None if enabled => Ok(None),
            None => Err(ApiError::Unauthorized(
                "This method requires an authenticated user".into(),
            )),
        }
    }
}

impl FromRequestParts<AppState> for MaybeUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if parts.headers.get(AUTHORIZATION).is_none() {
            return Ok(Self(None));
        }
        CurrentUser::from_request_parts(parts, state)
            .await
            .map(|user| Self(Some(user)))
    }
}

/// Resolves a raw bearer token to its user — the extractor's lookup,
/// callable where the token arrives outside the `Authorization` header
/// (the streaming handshake's query parameter / websocket subprotocol).
pub async fn user_for_token(state: &AppState, bearer: &str) -> Result<CurrentUser, ApiError> {
    user_for_token_with_ip(state, bearer, None).await
}

async fn user_for_token_with_ip(
    state: &AppState,
    bearer: &str,
    remote_addr: Option<std::net::IpAddr>,
) -> Result<CurrentUser, ApiError> {
    let token = oauth::find_active_token(&state.pool, &hash_secret(bearer))
        .await?
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    // A first-party browser session carries the advertised 90-day lifetime as a
    // hard server-side ceiling: once a web-app token ages past it, it stops
    // authenticating just like a revoked token, so a leaked session cookie
    // cannot outlive the window. Only the web app's own tokens
    // expire this way — third-party API tokens keep their non-expiring
    // semantics. A transient failure to resolve the web app id leaves the token
    // as-is rather than locking everyone out.
    if state
        .web_app_id()
        .await
        .is_ok_and(|web_app| web_app == token.app_id)
        && time::OffsetDateTime::now_utc() - token.created_at >= MAX_WEB_SESSION_AGE
    {
        return Err(ApiError::Unauthorized(INVALID_TOKEN.into()));
    }
    // Record this use for the active-sessions page, but only past the throttle
    // window — the common case (a token used within the last few minutes) skips
    // the write entirely by not even issuing the query.
    let should_touch = token
        .last_used_at
        .is_none_or(|used| time::OffsetDateTime::now_utc() - used >= oauth::TOUCH_INTERVAL);
    if should_touch {
        let ip = remote_addr.map(|ip| ip.to_string());
        let _ = oauth::touch_token(&state.pool, token.id, ip.as_deref()).await;
    }
    let user_id = token
        .user_id
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    let user = user::find_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    // A non-functional login's tokens stop resolving — Mastodon's
    // `require_user!` ladder, in its order and wording. The registration
    // e-mail endpoints use [`user_for_token_any_state`] instead.
    if !user.confirmed() {
        return Err(ApiError::Forbidden(
            "Your login is missing a confirmed e-mail address".into(),
        ));
    }
    if !user.approved {
        return Err(ApiError::Forbidden(
            "Your login is currently pending approval".into(),
        ));
    }
    if user.disabled {
        return Err(ApiError::Forbidden(
            "Your login is currently disabled".into(),
        ));
    }
    crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_addr).await?;
    if let Some(email) = user.email.as_deref() {
        crate::instance_policy::ensure_email_login_allowed(&state.pool, email).await?;
    }
    let account = account::find_by_id(&state.pool, user.account_id)
        .await?
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    // A suspension disables every existing bearer token immediately.  The
    // account row remains available to render the public suspended stub, but
    // it may no longer act locally until an administrator restores it.
    if account.suspended() {
        return Err(ApiError::Forbidden(
            "Your login is currently disabled".into(),
        ));
    }
    Ok(CurrentUser {
        user,
        account,
        scopes: token.scopes,
        app_id: token.app_id,
        token_id: token.id,
    })
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = bearer_from_parts(parts)?;
        let remote_addr = crate::instance_policy::client_ip(
            &parts.headers,
            crate::instance_policy::ip_from_connect_info(
                parts.extensions.get::<ConnectInfo<SocketAddr>>(),
            ),
            &state.config.trusted_proxies,
        );
        user_for_token_with_ip(state, bearer, remote_addr).await
    }
}

fn bearer_from_parts(parts: &Parts) -> Result<&str, ApiError> {
    parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))
}

/// Like [`CurrentUser`], but without the functional (confirmed / approved /
/// disabled) ladder — the registration e-mail endpoints
/// (`/api/v1/emails/*`) exist precisely for tokens of not-yet-functional
/// users, matching Mastodon's `require_authenticated_user!`-only routes.
pub struct AnyStateUser(pub CurrentUser);

impl FromRequestParts<AppState> for AnyStateUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = bearer_from_parts(parts)?;
        let token = oauth::find_active_token(&state.pool, &hash_secret(bearer))
            .await?
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        let user_id = token
            .user_id
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        let user = user::find_by_id(&state.pool, user_id)
            .await?
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        let account = account::find_by_id(&state.pool, user.account_id)
            .await?
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        Ok(Self(CurrentUser {
            user,
            account,
            scopes: token.scopes,
            app_id: token.app_id,
            token_id: token.id,
        }))
    }
}

/// The OAuth application behind an app-level (`client_credentials`) bearer
/// token — Mastodon's `require_client_credentials!`. A user-level token is
/// rejected with Mastodon's message.
pub struct AppToken {
    pub app: plamenu_db::oauth::App,
    pub scopes: String,
}

impl AppToken {
    /// Scope check with the same lattice semantics as [`CurrentUser`]
    /// ([`scopes_satisfy`]): broad grants cover narrow requirements, never the
    /// reverse.
    pub fn require_scope(&self, required: &str) -> Result<(), ApiError> {
        if scopes_satisfy(&self.scopes, required) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "This action is outside the authorized scopes".into(),
            ))
        }
    }
}

impl FromRequestParts<AppState> for AppToken {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = bearer_from_parts(parts)?;
        let token = oauth::find_active_token(&state.pool, &hash_secret(bearer))
            .await?
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        if token.user_id.is_some() {
            return Err(ApiError::Forbidden(
                // Mastodon's wording, grammar slip included.
                "This method requires an client credentials authentication".into(),
            ));
        }
        let app = oauth::find_app_by_id(&state.pool, token.app_id)
            .await?
            .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
        Ok(Self {
            app,
            scopes: token.scopes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_lattice_covers_broad_to_narrow_only() {
        // The cross-resource denial matrix. A broad family grant
        // covers itself and every granular requirement beneath it...
        assert!(scopes_satisfy("read", "read"));
        assert!(scopes_satisfy("read", "read:statuses"));
        assert!(scopes_satisfy("read", "read:notifications"));
        assert!(scopes_satisfy("write", "write:statuses"));
        assert!(scopes_satisfy("write", "write:media"));

        // ...but never a sibling family.
        assert!(!scopes_satisfy("read", "write"));
        assert!(!scopes_satisfy("read", "write:statuses"));
        assert!(!scopes_satisfy("read", "follow"));
        assert!(!scopes_satisfy("read", "push"));
        assert!(!scopes_satisfy("read", "profile"));

        // A granular grant is confined to its own resource: it must not widen
        // into the whole family (the finding's core bug — a `read:statuses`
        // token could read notifications) nor leak to a sibling resource.
        assert!(scopes_satisfy("read:statuses", "read:statuses"));
        assert!(!scopes_satisfy("read:statuses", "read"));
        assert!(!scopes_satisfy("read:statuses", "read:notifications"));
        assert!(!scopes_satisfy("write:accounts", "write"));
        assert!(!scopes_satisfy("write:accounts", "write:statuses"));

        // A whitespace-separated set satisfies a requirement if any single
        // grant does — and no combination of granular grants adds up to the
        // family.
        assert!(scopes_satisfy("read write", "read"));
        assert!(scopes_satisfy("read write", "write:media"));
        assert!(!scopes_satisfy("read write", "follow"));
        assert!(!scopes_satisfy("read:statuses write:statuses", "read"));
        assert!(!scopes_satisfy("read:statuses write:statuses", "write"));

        // Admin scopes obey the same lattice, including the resource tier, and
        // the user-family `read` never leaks into admin space.
        assert!(scopes_satisfy("admin:read", "admin:read:reports"));
        assert!(!scopes_satisfy("admin:read:reports", "admin:read"));
        assert!(!scopes_satisfy("admin:read:reports", "admin:read:accounts"));
        assert!(!scopes_satisfy("admin:read", "admin:write"));
        assert!(!scopes_satisfy("read", "admin:read"));
        assert!(!scopes_satisfy("read", "admin:read:reports"));

        // The boundary is the `:` delimiter: a prefix that is not a whole
        // segment must not match, in either direction.
        assert!(!scopes_satisfy("read", "readable"));
        assert!(!scopes_satisfy("re", "read"));
        assert!(!scopes_satisfy("", "read"));
        assert!(!scopes_satisfy("read", ""));
    }

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("hunter2!").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("hunter2!", &hash));
        assert!(!verify_password("hunter3!", &hash));
        assert!(!verify_password("hunter2!", "not-a-phc-string"));
    }

    #[test]
    fn dummy_hash_matches_real_verify_cost() {
        // The dummy hash carries the same algorithm and cost parameters as a
        // freshly minted one, so verifying against it costs the same as
        // verifying a real stored password — the property the enumeration fix
        // relies on.
        let real_hash = hash_password("hunter2!").unwrap();
        let real = PasswordHash::new(&real_hash).unwrap();
        let dummy = PasswordHash::new(&DUMMY_PASSWORD_HASH).unwrap();
        assert_eq!(real.algorithm, dummy.algorithm);
        assert_eq!(real.params, dummy.params);
        // Nothing matches the random dummy, so a dummy verification never
        // authenticates anyone.
        assert!(!verify_password("anything at all", &DUMMY_PASSWORD_HASH));
    }

    #[test]
    fn dummy_verify_spends_an_argon2_verification() {
        // The point of the dummy verify is the work it does: the counter must
        // advance, matching the wrong-password branch of a real login.
        let before = password_verify_count();
        verify_password_dummy("anything at all");
        assert!(password_verify_count() > before);
    }

    #[test]
    fn password_policy_enforces_the_shared_bounds() {
        // The single source of truth every credential surface now shares (#36).
        assert_eq!(validate_password(""), Err(PasswordPolicy::Empty));
        assert_eq!(
            validate_password(&"a".repeat(PASSWORD_MIN - 1)),
            Err(PasswordPolicy::TooShort)
        );
        assert_eq!(validate_password(&"a".repeat(PASSWORD_MIN)), Ok(()));
        assert_eq!(validate_password(&"a".repeat(PASSWORD_MAX)), Ok(()));
        assert_eq!(
            validate_password(&"a".repeat(PASSWORD_MAX + 1)),
            Err(PasswordPolicy::TooLong)
        );
    }

    #[test]
    fn password_policy_counts_unicode_scalars_not_bytes() {
        // The length the user perceives is scalar values, matching the
        // `minlength` the form hints. A seven-emoji password is 28 bytes — over
        // the 8-character floor by byte count — but only seven scalars, so it is
        // correctly rejected as too short rather than wrongly accepted.
        let seven_emoji = "😀".repeat(PASSWORD_MIN - 1);
        assert!(seven_emoji.len() > PASSWORD_MIN); // a byte counter would accept it
        assert_eq!(
            validate_password(&seven_emoji),
            Err(PasswordPolicy::TooShort)
        );
        assert_eq!(validate_password(&"😀".repeat(PASSWORD_MIN)), Ok(()));
    }

    #[test]
    fn secrets_are_unique_and_hashes_deterministic() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43); // 32 bytes, base64url, no padding
        assert_eq!(hash_secret(&a), hash_secret(&a));
        assert_ne!(hash_secret(&a), hash_secret(&b));
        assert!(secret_hash_eq(&hash_secret(&a), &hash_secret(&a)));
        assert!(!secret_hash_eq(&hash_secret(&a), &hash_secret(&b)));
        assert!(!secret_hash_eq(&hash_secret(&a), "short"));
    }

    #[test]
    fn same_origin_accepts_our_origin_and_rejects_foreign() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let domain = "plamenu.test";

        // Our own Origin passes.
        let mut ours = HeaderMap::new();
        ours.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://plamenu.test"),
        );
        assert!(same_origin_request(&ours, domain));

        // A foreign origin — the shape a cross-site login CSRF takes — is
        // refused, as is an opaque `null` origin and a lookalike host that
        // merely shares our prefix.
        for foreign in [
            "https://evil.example",
            "null",
            "https://plamenu.test.evil.example",
            "http://plamenu.test",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, HeaderValue::from_str(foreign).unwrap());
            assert!(!same_origin_request(&headers, domain), "rejects {foreign}");
        }
    }

    #[test]
    fn same_origin_falls_back_to_referer_then_allows_when_absent() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let domain = "plamenu.test";

        // No Origin, but a same-origin Referer → allowed.
        let mut ours = HeaderMap::new();
        ours.insert(
            header::REFERER,
            HeaderValue::from_static("https://plamenu.test/login"),
        );
        assert!(same_origin_request(&ours, domain));

        // A foreign referrer, or a userinfo-smuggling lookalike, → refused.
        for foreign in [
            "https://evil.example/login",
            "https://plamenu.test@evil.example/login",
            "not-a-url",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::REFERER, HeaderValue::from_str(foreign).unwrap());
            assert!(!same_origin_request(&headers, domain), "rejects {foreign}");
        }

        // Neither header (a non-browser client) → allowed: no ambient session
        // to swap, so there is nothing to protect.
        assert!(same_origin_request(&HeaderMap::new(), domain));
    }

    #[test]
    fn pkce_s256_known_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
