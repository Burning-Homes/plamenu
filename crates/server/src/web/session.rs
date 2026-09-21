//! Cookie-backed sessions for the first-party web UI.
//!
//! The browser UI authenticates with the same OAuth tokens the API uses: a
//! built-in first-party app (`ensure_web_app`) owns them, and on sign-in we
//! mint a token and hand it to the browser in an `HttpOnly` cookie instead of
//! a JSON body. Every web page then resolves that cookie through the ordinary
//! `user_for_token` path, so the UI and the API share one auth model.

use std::net::IpAddr;

use axum::extract::{Form, FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use maud::{Markup, html};
use plamenu_db::oauth::{self, App, NewApp};
use plamenu_db::two_factor;
use plamenu_db::user;
use serde::Deserialize;

use super::clock::ViewerClock;
use super::i18n::Locale;
use super::layout;
use crate::auth::{CurrentUser, generate_secret, hash_secret, user_for_token};
use crate::instance_policy::RemoteIp;
use crate::state::AppState;

/// Cookie carrying the session token. The `__Host-` prefix is honoured by
/// browsers only when the cookie is `Secure`, `Path=/` and host-scoped (no
/// `Domain`), which is exactly how we set it — Plamenu is always served over
/// HTTPS (behind Caddy in dev, TLS in staging/prod).
const COOKIE_NAME: &str = "__Host-plamenu_session";

/// Stable `client_id` of the built-in browser app. Bootstrapped on first
/// login and reused thereafter.
const WEB_CLIENT_ID: &str = "plamenu-web-ui";

/// Scopes the web session is granted — everything the UI can drive.
const WEB_SCOPES: &str = "read write follow push";

/// Finds (or creates, once) the first-party app that owns web sessions.
///
/// Idempotent: a unique `client_id` makes the create a no-op on every login
/// after the first. The client secret is random and unused — the web UI mints
/// tokens directly on password sign-in rather than through the code flow.
pub async fn ensure_web_app(state: &AppState) -> Result<App, crate::error::ApiError> {
    if let Some(app) = oauth::find_app_by_client_id(&state.pool, WEB_CLIENT_ID).await? {
        return Ok(app);
    }
    let app = oauth::create_app(
        &state.pool,
        NewApp {
            name: "Plamenu",
            website: None,
            client_id: WEB_CLIENT_ID,
            client_secret_hash: &hash_secret(&generate_secret()),
            redirect_uris: &[],
            scopes: WEB_SCOPES,
        },
    )
    .await?;
    Ok(app)
}

/// Reads a single cookie value out of the request's `Cookie` header.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim_start().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// The raw bearer-equivalent token carried by the first-party web session
/// cookie. API-web compatibility routes use this to share the browser session
/// with Mastodon's `/api/web/*` surface.
pub(crate) fn raw_session_token(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, COOKIE_NAME)
}

/// How long the session cookie persists. Without an explicit `Max-Age` the
/// cookie is a *session* cookie — dropped when the browser closes, which mobile
/// browsers do aggressively, logging the user out far too often. The backing
/// OAuth token now carries a matching server-side lifetime
/// ([`crate::auth::MAX_WEB_SESSION_AGE`]), so the advertised
/// window is real on both ends rather than an indefinitely-live token behind a
/// cosmetic cookie expiry. 90 days.
const COOKIE_MAX_AGE: i64 = 90 * 24 * 60 * 60;

/// `Set-Cookie` value that installs the session token.
fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={COOKIE_MAX_AGE}"
    )
}

/// `Set-Cookie` value that clears the session.
fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub(crate) fn clear_session_cookie() -> String {
    clear_cookie()
}

// ---- Multi-account roster ----------------------------------------------
//
// The browser can be signed into several accounts at once. The *active* one
// lives in `__Host-plamenu_session` (above) and drives every page unchanged;
// the full set — so the sign-in screen can offer to switch — lives in a second
// `__Host-plamenu_accounts` cookie. Each entry is `{account_id}.{token}`,
// entries joined by `~`; both delimiters sit outside the base64url token
// alphabet, so no entry is ambiguous.

/// Cookie carrying the multi-account roster. Same `__Host-`/`Secure`/`HttpOnly`
/// posture as the session cookie — it holds bearer-equivalent tokens, exactly
/// like the session cookie already does.
const ROSTER_COOKIE: &str = "__Host-plamenu_accounts";

/// Upper bound on accounts signed in at once, bounding the cookie size and
/// casual abuse. Adding past this drops the least-recently-added entry *and*
/// revokes its token, so an evicted account leaves no orphaned
/// live session behind.
const MAX_ACCOUNTS: usize = 5;

/// One signed-in account: the local account id (non-secret, the switch target)
/// paired with its session token.
#[derive(Clone)]
struct RosterEntry {
    account_id: i64,
    token: String,
}

/// Parses the roster cookie; malformed entries are skipped rather than failing
/// the whole roster (a partial roster still lets the user switch).
fn parse_roster(raw: &str) -> Vec<RosterEntry> {
    raw.split('~')
        .filter_map(|entry| {
            let (id, token) = entry.split_once('.')?;
            let account_id = id.parse().ok()?;
            (!token.is_empty()).then(|| RosterEntry {
                account_id,
                token: token.to_owned(),
            })
        })
        .collect()
}

/// The roster carried by the request, empty when the cookie is absent.
fn read_roster(headers: &HeaderMap) -> Vec<RosterEntry> {
    cookie_value(headers, ROSTER_COOKIE)
        .map(parse_roster)
        .unwrap_or_default()
}

/// `Set-Cookie` value that installs the roster.
fn set_roster_cookie(roster: &[RosterEntry]) -> String {
    let value = roster
        .iter()
        .map(|e| format!("{}.{}", e.account_id, e.token))
        .collect::<Vec<_>>()
        .join("~");
    format!(
        "{ROSTER_COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={COOKIE_MAX_AGE}"
    )
}

/// `Set-Cookie` value that clears the roster.
fn clear_roster_cookie() -> String {
    format!("{ROSTER_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// A CSRF token bound to the roster cookie — the account-chooser counterpart of
/// [`csrf_token`]. Keying it to the roster (not the session) means the switch
/// form is still guarded on the post-sign-out chooser, where there is no
/// session token to derive from.
fn roster_csrf(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, ROSTER_COOKIE).map(csrf_token)
}

/// Selects one live account from the browser's multi-account roster for an
/// OAuth authorization request and returns the active-session cookie to set.
/// The roster cookie is bearer-equivalent, so the same roster-bound CSRF token
/// used by the ordinary account switcher guards this path as well.
pub(crate) async fn oauth_switch_cookie(
    state: &AppState,
    headers: &HeaderMap,
    account_id: i64,
    submitted_csrf: &str,
) -> Result<String, crate::error::ApiError> {
    let expected = roster_csrf(headers)
        .ok_or_else(|| crate::error::ApiError::Unauthorized("No signed-in account found".into()))?;
    if submitted_csrf != expected {
        return Err(crate::error::ApiError::Forbidden(
            "invalid CSRF token".into(),
        ));
    }
    let entry = read_roster(headers)
        .into_iter()
        .find(|entry| entry.account_id == account_id)
        .ok_or_else(|| crate::error::ApiError::Unauthorized("No signed-in account found".into()))?;
    let current = user_for_token(state, &entry.token).await?;
    if current.account.id != account_id {
        return Err(crate::error::ApiError::Unauthorized(
            "No signed-in account found".into(),
        ));
    }
    Ok(set_cookie(&entry.token))
}

/// The account behind the current active-session cookie, as a roster entry.
/// Used to fold a pre-existing single-cookie session into the roster the first
/// time a second account is added.
async fn current_active_entry(state: &AppState, headers: &HeaderMap) -> Option<RosterEntry> {
    let token = cookie_value(headers, COOKIE_NAME)?.to_owned();
    let current = user_for_token(state, &token).await.ok()?;
    Some(RosterEntry {
        account_id: current.account.id,
        token,
    })
}

/// Builds the pair of `Set-Cookie` values that install `new_token` as the active
/// session and fold it into the roster: the prior active account is preserved,
/// the account is deduped (a re-login supersedes and revokes its old token), and
/// the roster is capped with the active account always retained.
async fn session_cookies(
    state: &AppState,
    app: &App,
    headers: &HeaderMap,
    user_id: i64,
    new_token: String,
) -> (String, String) {
    let Some(new_account_id) = user::find_by_id(&state.pool, user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.account_id)
    else {
        // Can't resolve the account (should not happen right after auth): fall
        // back to a plain single-account session, leaving any roster untouched.
        return (
            set_cookie(&new_token),
            set_roster_cookie(&read_roster(headers)),
        );
    };

    let mut roster = read_roster(headers);
    // Keep the account already signed in (covers sessions minted before the
    // roster cookie existed, and the "add another account" flow, where the
    // request still carries the previous active cookie).
    if let Some(prev) = current_active_entry(state, headers).await
        && !roster.iter().any(|e| e.account_id == prev.account_id)
    {
        roster.push(prev);
    }
    // Upsert the newly authenticated account.
    if let Some(existing) = roster.iter_mut().find(|e| e.account_id == new_account_id) {
        if existing.token != new_token {
            // The account's previous session is superseded — revoke it so a
            // re-login doesn't leave an orphaned live token behind. A failed
            // revocation must not be silent: the superseded
            // token would otherwise stay live with no retry path.
            if let Err(error) =
                oauth::revoke_token(&state.pool, &hash_secret(&existing.token), app.id).await
            {
                tracing::warn!(%error, "re-login: superseded token revocation failed");
            }
            existing.token.clone_from(&new_token);
        }
    } else {
        roster.push(RosterEntry {
            account_id: new_account_id,
            token: new_token.clone(),
        });
    }
    // Sort the active account last so the cap only ever drops older entries.
    if let Some(pos) = roster.iter().position(|e| e.account_id == new_account_id) {
        let active = roster.remove(pos);
        roster.push(active);
    }
    while roster.len() > MAX_ACCOUNTS {
        // Dropping the oldest account from the switcher must also revoke its
        // token, or the evicted session lingers as a live credential the browser
        // no longer tracks and can no longer sign out. Same
        // posture as the supersession path above: a failed revocation is logged
        // loudly, never swallowed.
        let evicted = roster.remove(0);
        if let Err(error) =
            oauth::revoke_token(&state.pool, &hash_secret(&evicted.token), app.id).await
        {
            tracing::warn!(%error, "roster eviction: token revocation failed");
        }
    }
    (set_cookie(&new_token), set_roster_cookie(&roster))
}

/// After a credential change that revokes every existing token for `user_id`
/// (a signed-in password change), mint a fresh session token for
/// *this* browser and return the `Set-Cookie` pair that installs it.
///
/// The point is that the user who just changed their password stays signed in
/// on the device they changed it from, while every other session and every app
/// token — including a copy of the *current* session token an intruder may hold
/// — is dead. Call this only *after* revoking the old tokens: the new token is
/// inserted fresh, so it survives the revocation. The roster's entry for this
/// account is rotated onto the new token (its now-revoked old token being a
/// no-op to re-revoke); any other rostered accounts belong to different users,
/// are untouched by the revoke, and keep their sessions.
pub(crate) async fn reissue_session(
    state: &AppState,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
    user_id: i64,
) -> Result<(String, String), crate::error::ApiError> {
    let app = ensure_web_app(state).await?;
    let raw = generate_secret();
    let user_agent = crate::auth::user_agent_string(headers);
    let ip = remote_ip.map(|ip| ip.to_string());
    oauth::create_token_with_meta(
        &state.pool,
        &hash_secret(&raw),
        app.id,
        Some(user_id),
        WEB_SCOPES,
        oauth::SessionMeta {
            user_agent: user_agent.as_deref(),
            ip: ip.as_deref(),
        },
    )
    .await?;
    Ok(session_cookies(state, &app, headers, user_id, raw).await)
}

/// A stateless CSRF token bound to the session: deriving it from the secret
/// session token means an attacker who cannot read the cookie cannot forge it,
/// and we never have to store it. Defence in depth on top of `SameSite=Lax`.
fn csrf_token(raw_token: &str) -> String {
    hash_secret(&format!("plamenu-csrf:{raw_token}"))
}

/// An authenticated browser session: the resolved user plus the CSRF token to
/// embed in any state-changing form.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent nav/permission flags"
)]
pub struct WebUser {
    pub current: CurrentUser,
    pub csrf: String,
    /// The viewer's resolved role. Kept on the session so client renderers can
    /// expose only the admin shortcuts their exact permission set can open,
    /// and so the admin extractor does not have to load the same row again.
    pub(crate) role: Option<plamenu_db::role::Role>,
    /// Negotiated interface locale. The stored user preference takes
    /// precedence over this request's `Accept-Language` header.
    pub locale: Locale,
    /// How this viewer reads a timestamp: their stored zone plus the
    /// negotiated locale. Resolved once per request from the same row the
    /// locale comes from, so every page — including the admin console, which
    /// has no settings context to hang it on — can render viewer-local times
    /// without a query of its own.
    pub clock: ViewerClock,
    /// Whether the account holds any moderation role — drives the admin link
    /// in the navigation chrome.
    pub is_staff: bool,
    /// Whether the account's role grants `invite_users` — drives the Invites
    /// settings tab and gates the invite endpoints.
    pub can_invite: bool,
    /// Whether notifications newer than the `notifications` marker exist —
    /// drives the unread dot on the navigation bell.
    pub unread_notifications: bool,
    /// Whether any (unmuted) conversation is unread — drives the unread dot
    /// on the Private-mentions nav entry and the mobile drawer toggle.
    pub unread_conversations: bool,
    /// Built-in client's live notification stream and its optional cue.
    pub live_notifications: bool,
    pub notification_sound: bool,
    pub notification_volume: u8,
}

impl WebUser {
    /// Whether the viewer's role grants one exact permission. The
    /// administrator short-circuit stays centralized in [`Role::can`].
    #[must_use]
    pub fn can(&self, permission: i64) -> bool {
        self.role.as_ref().is_some_and(|role| role.can(permission))
    }

    /// The small target-navigation capability set shared by status and
    /// profile privileged-tools menus.
    #[must_use]
    pub fn admin_capabilities(&self) -> AdminCapabilities {
        use plamenu_db::role::permission;
        AdminCapabilities {
            manage_users: self.can(permission::MANAGE_USERS),
            manage_reports: self.can(permission::MANAGE_REPORTS),
            manage_federation: self.can(permission::MANAGE_FEDERATION),
            manage_groups: self.can(permission::MANAGE_GROUPS),
            manage_custom_emojis: self.can(permission::MANAGE_CUSTOM_EMOJIS),
            personal_custom_emojis: self.can(permission::UPLOAD_CUSTOM_EMOJIS),
        }
    }

    /// Whether a submitted CSRF token matches this session.
    #[must_use]
    pub fn csrf_ok(&self, submitted: &str) -> bool {
        submitted == self.csrf
    }
}

/// The response for a rejected (missing or mismatched) CSRF token.
#[must_use]
pub fn csrf_rejection() -> Response {
    (StatusCode::FORBIDDEN, "invalid CSRF token").into_response()
}

/// The response for a pre-session credential POST that did not demonstrably
/// come from our own site — the anti-login-CSRF guard shared by the sign-in and
/// second-factor submits. A signed-in mutation uses
/// [`csrf_rejection`] instead; the sign-in transition has no session to key a
/// token to, so [`crate::auth::same_origin_request`] is the gate here.
#[must_use]
pub(crate) fn cross_origin_login_rejection() -> Response {
    (
        StatusCode::FORBIDDEN,
        "This sign-in request did not come from this site.",
    )
        .into_response()
}

/// Optional session: `None` when there is no valid session cookie. Used by
/// pages that render for both signed-in and anonymous visitors.
pub struct MaybeWebUser(pub Option<WebUser>);

/// Gate a preview page: anonymous visitors are bounced to `/login` unless the
/// page is `enabled`, signed-in visitors always pass. Returns the redirect to
/// hand back, or `None` when access is allowed.
#[must_use]
pub fn preview_redirect(session: &Option<WebUser>, enabled: bool) -> Option<Response> {
    (session.is_none() && !enabled).then(|| Redirect::to("/login").into_response())
}

async fn resolve(parts: &mut Parts, state: &AppState) -> Option<WebUser> {
    let raw = cookie_value(&parts.headers, COOKIE_NAME)?.to_owned();
    let current = user_for_token(state, &raw).await.ok()?;
    let (preferred_locale, stored_zone, allow_direct_media) =
        user::web_preferences(&state.pool, current.user.id)
            .await
            .unwrap_or((None, None, false));
    if allow_direct_media
        && let Some(security) = parts.extensions.get::<crate::RequestSecurityContext>()
    {
        security.allow_direct_remote_media();
    }
    let accepted = parts
        .headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok());
    let locale = Locale::negotiate(preferred_locale.as_deref(), accepted);
    let clock = ViewerClock::of(stored_zone.as_deref(), locale);
    // A privileged moderation role grants the admin link — the same test the
    // dashboard extractor applies, so the link never leads to a 403. A role
    // granting only `invite_users` doesn't count as staff.
    let role = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await
        .ok()
        .flatten();
    let is_staff = role
        .as_ref()
        .is_some_and(plamenu_db::role::Role::privileged);
    let can_invite = role
        .as_ref()
        .is_some_and(|role| role.can(plamenu_db::role::permission::INVITE_USERS));
    // The navigation bell's unread dot. Chrome, not content: a failed probe
    // renders as "nothing new" rather than failing the page.
    let unread_notifications =
        plamenu_db::notification::has_unread(&state.pool, current.user.id, current.account.id)
            .await
            .unwrap_or(false);
    let unread_conversations =
        plamenu_db::conversation::has_unread(&state.pool, current.account.id)
            .await
            .unwrap_or(false);
    let notification_preferences =
        plamenu_db::web_setting::notification_preferences(&state.pool, current.user.id)
            .await
            .unwrap_or_default();
    Some(WebUser {
        current,
        csrf: csrf_token(&raw),
        role,
        locale,
        clock,
        is_staff,
        can_invite,
        unread_notifications,
        unread_conversations,
        live_notifications: notification_preferences.live_updates,
        notification_sound: notification_preferences.sound,
        notification_volume: notification_preferences.volume,
    })
}

/// The exact read/navigation permissions needed by contextual admin menus.
/// Keeping this narrower than `is_staff` prevents a reports-only moderator,
/// for example, from receiving account or federation links that return 403.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a compact independent permission projection, not coupled state"
)]
pub struct AdminCapabilities {
    pub manage_users: bool,
    pub manage_reports: bool,
    pub manage_federation: bool,
    pub manage_groups: bool,
    pub manage_custom_emojis: bool,
    pub personal_custom_emojis: bool,
}

impl FromRequestParts<AppState> for MaybeWebUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(resolve(parts, state).await))
    }
}

impl FromRequestParts<AppState> for WebUser {
    /// A missing or invalid session sends the browser to the sign-in page
    /// rather than returning the API's JSON 401.
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        resolve(parts, state)
            .await
            .ok_or_else(|| Redirect::to("/login"))
    }
}

#[derive(Deserialize)]
pub struct LoginQuery {
    /// Presence (`?switch=1`) forces the account chooser even for a signed-in
    /// visitor — the "Switch account" affordance.
    switch: Option<String>,
}

/// `GET /login` — the sign-in form, or the account chooser when this browser is
/// already signed into one or more accounts.
///
/// A plain visit while signed in is bounced home; `?switch=1` (or landing here
/// right after signing out with other accounts still held) renders the chooser
/// instead.
pub async fn login_form(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    locale: Locale,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    if session.is_some() && query.switch.is_none() {
        return Redirect::to("/").into_response();
    }
    let accounts = chooser_accounts(&state, &headers, session.as_ref()).await;
    let mascot = mascot_url(&state).await;
    let signup_open = signup_open(&state).await;
    if accounts.is_empty() {
        return login_page(&state, mascot.as_deref(), None, signup_open, locale)
            .await
            .into_response();
    }
    chooser_page(
        &state,
        &headers,
        session.as_ref(),
        &accounts,
        mascot.as_deref(),
        signup_open,
        locale,
    )
    .await
    .into_response()
}

/// One account offered on the chooser: its serialized entity (for the avatar and
/// name) plus whether it is the currently active session.
struct ChooserAccount {
    account_id: i64,
    value: serde_json::Value,
    is_current: bool,
}

/// The accounts this browser can choose between: the roster, plus the active
/// account (so a pre-roster session still lists itself), each rendered from a
/// freshly serialized entity.
async fn chooser_accounts(
    state: &AppState,
    headers: &HeaderMap,
    session: Option<&WebUser>,
) -> Vec<ChooserAccount> {
    let current_id = session.map(|s| s.current.account.id);
    let mut ids: Vec<i64> = Vec::new();
    for entry in read_roster(headers) {
        if !ids.contains(&entry.account_id) {
            ids.push(entry.account_id);
        }
    }
    if let Some(id) = current_id
        && !ids.contains(&id)
    {
        ids.push(id);
    }
    if ids.is_empty() {
        return Vec::new();
    }
    // The write path caps the roster at MAX_ACCOUNTS; the cookie is
    // client-controlled, so re-apply the cap on read (keeping room for the
    // active account appended above) before rendering anything.
    ids.truncate(MAX_ACCOUNTS + 1);
    // One batched render for the whole roster instead of a fresh
    // `account_json` (several queries) per entry.
    let mut accounts = plamenu_db::account::find_by_ids(&state.pool, &ids)
        .await
        .unwrap_or_default();
    accounts.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    let rendered =
        crate::entities::render_accounts(&state.pool, &state.config.domain, &accounts, None)
            .await
            .unwrap_or_default();
    accounts
        .iter()
        .zip(rendered)
        .map(|(account, value)| ChooserAccount {
            account_id: account.id,
            value,
            is_current: Some(account.id) == current_id,
        })
        .collect()
}

/// The account chooser: a card per signed-in account (the active one marked,
/// the rest switch forms) above a collapsed "sign in to another account" form.
async fn chooser_page(
    state: &AppState,
    headers: &HeaderMap,
    session: Option<&WebUser>,
    accounts: &[ChooserAccount],
    mascot: Option<&str>,
    signup_open: bool,
    request_locale: Locale,
) -> Markup {
    let locale = session.map_or(request_locale, |user| user.locale);
    let csrf = roster_csrf(headers);
    let content = html! {
        section.auth-card {
            @if let Some(src) = mascot {
                img.auth-card__mascot src=(src) alt="";
            }
            h1 { (locale.text("auth-choose-account")) }
            ul.account-chooser {
                @for entry in accounts {
                    @let account = super::view::Account(&entry.value);
                    li.account-chooser__item {
                        @if entry.is_current {
                            a.account-card.account-card--current href="/" {
                                img.account-card__avatar src=(account.avatar()) alt=""
                                    width="40" height="40" loading="lazy";
                                span.account-card__body {
                                    span.account-card__name { (account.name_markup()) }
                                    span.account-card__acct { (account.handle_prefix()) (account.acct()) }
                                }
                                span.account-chooser__badge { (locale.text("auth-current-account")) }
                            }
                        } @else {
                            form.account-chooser__switch method="post" action="/web/accounts/switch" {
                                @if let Some(token) = &csrf {
                                    input type="hidden" name="csrf" value=(token);
                                }
                                input type="hidden" name="account_id" value=(entry.account_id);
                                button.account-card.account-card--button type="submit" {
                                    img.account-card__avatar src=(account.avatar()) alt=""
                                        width="40" height="40" loading="lazy";
                                    span.account-card__body {
                                        span.account-card__name { (account.name_markup()) }
                                        span.account-card__acct { (account.handle_prefix()) (account.acct()) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            details.auth-add-account {
                summary { (locale.text("auth-another-account")) }
                form.auth-form method="post" action="/login" {
                    label {
                        (locale.text("auth-identifier"))
                        input type="text" name="identifier" autocomplete="username" required;
                    }
                    label {
                        (locale.text("auth-password"))
                        input type="password" name="password"
                            autocomplete="current-password" required;
                    }
                    button type="submit" { (locale.text("nav-sign-in")) }
                }
            }
            @if crate::mailer::enabled(state) {
                p.auth-card__alt { a href="/auth/password/new" { (locale.text("auth-forgot-password")) } }
            }
            @if signup_open {
                p.auth-card__alt {
                    (locale.text("auth-no-account")) " "
                    a href="/signup" { (locale.text("auth-sign-up")) }
                }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("auth-choose-account"),
        session,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

/// Whether to advertise the sign-up page in the auth chrome.
async fn signup_open(state: &AppState) -> bool {
    crate::registration::open_for_registrations(state)
        .await
        .unwrap_or(false)
}

/// The operator's mascot upload, shown on the sign-in card (the web
/// counterpart of Mastodon's mascot setting, which its frontend drawer
/// displays).
async fn mascot_url(state: &AppState) -> Option<String> {
    let mascot = plamenu_db::site_upload::get(&state.pool, "mascot")
        .await
        .ok()??;
    Some(format!(
        "https://{}/media/{}",
        state.config.domain, mascot.file_name
    ))
}

async fn login_page(
    state: &AppState,
    mascot: Option<&str>,
    error: Option<&str>,
    signup_open: bool,
    locale: Locale,
) -> Markup {
    let content = html! {
        section.auth-card {
            @if let Some(src) = mascot {
                img.auth-card__mascot src=(src) alt="";
            }
            h1 { (locale.text("nav-sign-in")) }
            @if let Some(message) = error {
                p.form-error id="login-error" role="alert" { (message) }
            }
            form.auth-form method="post" action="/login" {
                label {
                    (locale.text("auth-identifier"))
                    input type="text" name="identifier" autocomplete="username"
                        required autofocus aria-invalid=[error.map(|_| "true")]
                        aria-describedby=[error.map(|_| "login-error")];
                }
                label {
                    (locale.text("auth-password"))
                    input type="password" name="password"
                        autocomplete="current-password" required
                        aria-invalid=[error.map(|_| "true")]
                        aria-describedby=[error.map(|_| "login-error")];
                }
                button type="submit" { (locale.text("nav-sign-in")) }
            }
            @if crate::mailer::enabled(state) {
                p.auth-card__alt { a href="/auth/password/new" { (locale.text("auth-forgot-password")) } }
            }
            @if signup_open {
                p.auth-card__alt {
                    (locale.text("auth-no-account")) " "
                    a href="/signup" { (locale.text("auth-sign-up")) }
                }
            }
        }
    };
    // Bare-domain shares land here (anonymous `/` redirects to `/login`), so
    // the sign-in page carries the instance-level preview card. Metadata is
    // best-effort chrome: a failed load falls back to none.
    let meta = super::meta::instance_page(state, "/", locale)
        .await
        .unwrap_or_default();
    layout::shell_visitor_subject_localized(
        &locale.text("nav-sign-in"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        &meta,
        locale,
    )
}

/// The second-factor prompt shown after a correct password on a 2FA account.
/// One input accepts a TOTP code or a recovery code; the raw challenge token
/// round-trips in a hidden field. Reused verbatim after a wrong code, so it
/// takes an optional error.
///
/// The TOTP form is always present (the no-JS fallback). When the account has a
/// security key registered, a JS-revealed "Use a security key" affordance is
/// added; it stays `hidden` without JavaScript, so nothing breaks when the
/// browser cannot drive a `WebAuthn` ceremony.
pub(crate) async fn challenge_page(
    state: &AppState,
    challenge_token: &str,
    error: Option<&str>,
    show_webauthn: bool,
    locale: Locale,
) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("auth-two-factor")) }
            @if let Some(message) = error {
                p.form-error id="challenge-error" role="alert" { (message) }
            }
            @if show_webauthn {
                (super::webauthn::login_prompt(challenge_token, None, locale))
            }
            p.auth-card__alt {
                (locale.text("auth-code-help"))
            }
            form.auth-form method="post" action="/login/challenge" {
                input type="hidden" name="challenge_token" value=(challenge_token);
                label {
                    (locale.text("auth-code"))
                    input type="text" name="code" inputmode="numeric"
                        autocomplete="one-time-code" autofocus required
                        aria-invalid=[error.map(|_| "true")]
                        aria-describedby=[error.map(|_| "challenge-error")];
                }
                button type="submit" { (locale.text("auth-verify")) }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("auth-two-factor"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

/// The credential form used when an OAuth request has no reusable account, or
/// when the user deliberately chooses to sign in as another account.
fn oauth_credentials(hidden: &[(&str, &str)], locale: Locale) -> Markup {
    html! {
        form.auth-form method="post" action="/oauth/authorize" {
            @for (name, value) in hidden {
                input type="hidden" name=(name) value=(value);
            }
            label {
                (locale.text("auth-identifier"))
                input type="text" name="identifier" autocomplete="username" required;
            }
            label {
                (locale.text("auth-password"))
                input type="password" name="password"
                    autocomplete="current-password" required;
            }
            button type="submit" { (locale.text("consent-sign-in")) }
        }
    }
}

fn oauth_add_account(hidden: &[(&str, &str)], locale: Locale) -> Markup {
    html! {
        details.auth-add-account {
            summary { (locale.text("auth-another-account")) }
            (oauth_credentials(hidden, locale))
        }
    }
}

fn oauth_current_account(entry: &ChooserAccount, locale: Locale) -> Markup {
    let account = super::view::Account(&entry.value);
    html! {
        div.account-card.account-card--current {
            img.account-card__avatar src=(account.avatar()) alt=""
                width="40" height="40" loading="lazy";
            span.account-card__body {
                span.account-card__name { (account.name_markup()) }
                span.account-card__acct { (account.handle_prefix()) (account.acct()) }
            }
            span.account-chooser__badge { (locale.text("auth-current-account")) }
        }
    }
}

fn oauth_consent_actions(user: &WebUser, hidden: &[(&str, &str)], locale: Locale) -> Markup {
    html! {
        div.consent-actions {
            form method="post" action="/oauth/authorize" {
                @for (name, value) in hidden {
                    input type="hidden" name=(name) value=(value);
                }
                input type="hidden" name="decision" value="allow";
                input type="hidden" name="csrf" value=(user.csrf);
                button type="submit" { (locale.text("consent-authorize")) }
            }
            form method="post" action="/oauth/authorize" {
                @for (name, value) in hidden {
                    input type="hidden" name=(name) value=(value);
                }
                input type="hidden" name="decision" value="deny";
                input type="hidden" name="csrf" value=(user.csrf);
                button.settings-button--plain type="submit" {
                    (locale.text("consent-deny"))
                }
            }
        }
    }
}

/// Renders every non-current roster entry as an OAuth-preserving switch form.
/// With no active session every entry is non-current, so this is also the
/// anonymous account chooser.
fn oauth_switch_accounts(
    accounts: &[ChooserAccount],
    hidden: &[(&str, &str)],
    switch_csrf: Option<&str>,
) -> Markup {
    html! {
        ul.account-chooser {
            @for entry in accounts.iter().filter(|entry| !entry.is_current) {
                @let account = super::view::Account(&entry.value);
                li.account-chooser__item {
                    form.account-chooser__switch method="post" action="/oauth/authorize" {
                        @for (name, value) in hidden {
                            input type="hidden" name=(name) value=(value);
                        }
                        input type="hidden" name="decision" value="switch";
                        input type="hidden" name="account_id" value=(entry.account_id);
                        @if let Some(token) = switch_csrf {
                            input type="hidden" name="roster_csrf" value=(token);
                        }
                        button.account-card.account-card--button type="submit" {
                            img.account-card__avatar src=(account.avatar()) alt=""
                                width="40" height="40" loading="lazy";
                            span.account-card__body {
                                span.account-card__name { (account.name_markup()) }
                                span.account-card__acct {
                                    (account.handle_prefix()) (account.acct())
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The OAuth consent screen for `GET /oauth/authorize`.
///
/// A valid browser session gets a real allow / deny decision without spending
/// credentials again. The browser's existing multi-account roster is offered
/// here too; choosing an account switches the active session and returns to
/// this same request. `hidden` carries the authorization parameters that must
/// round-trip into the POST, with maud escaping every value.
pub(crate) async fn consent_page(
    state: &AppState,
    headers: &HeaderMap,
    session: Option<&WebUser>,
    app_name: &str,
    scopes: &str,
    hidden: &[(&str, &str)],
    request_locale: Locale,
) -> Markup {
    let locale = session.map_or(request_locale, |user| user.locale);
    let accounts = chooser_accounts(state, headers, session).await;
    let switch_csrf = roster_csrf(headers);
    let content = html! {
        section.auth-card {
            h1 { (locale.text("consent-title")) }
            p.consent-lead {
                // The application's name is part of the sentence, so the whole
                // sentence is one message with the name emphasized inside it.
                (locale.markup(
                    "consent-lead",
                    &[("app", html! { strong { (app_name) } })],
                ))
            }
            @if scopes.split_whitespace().next().is_some() {
                ul.consent-scopes {
                    @for scope in scopes.split_whitespace() {
                        li { code { (scope) } }
                    }
                }
            }
            @if let Some(user) = session {
                p.auth-card__alt { (locale.text("consent-authorizing-as")) }
                @if let Some(entry) = accounts.iter().find(|entry| entry.is_current) {
                    (oauth_current_account(entry, locale))
                }
                (oauth_consent_actions(user, hidden, locale))
                @if accounts.iter().any(|entry| !entry.is_current) {
                    details.auth-add-account {
                        summary { (locale.text("consent-switch-account")) }
                        (oauth_switch_accounts(&accounts, hidden, switch_csrf.as_deref()))
                    }
                }
                (oauth_add_account(hidden, locale))
            } @else if !accounts.is_empty() {
                h2 { (locale.text("auth-choose-account")) }
                (oauth_switch_accounts(&accounts, hidden, switch_csrf.as_deref()))
                (oauth_add_account(hidden, locale))
            } @else {
                (oauth_credentials(hidden, locale))
            }
        }
    };
    layout::focused_shell_localized(&locale.text("consent-page-title"), &content, locale)
}

/// The OAuth second-factor prompt: like [`consent_page`], but the credentials
/// have already been checked and we now need the TOTP or recovery code. The
/// authorization-request parameters plus the challenge token round-trip as
/// hidden fields back into `POST /oauth/authorize`.
///
/// When the account has a security key registered, a JS-revealed "Use a security
/// key" affordance is added — the same shim as the web sign-in, pointed at the
/// OAuth ceremony endpoints. It forwards the form's hidden authorization
/// parameters so the assertion can mint the grant. Without JavaScript it stays
/// `hidden` and the TOTP form remains the fallback.
pub(crate) fn oauth_challenge_page(
    hidden: &[(&str, &str)],
    challenge_token: &str,
    error: Option<&str>,
    show_webauthn: bool,
    locale: Locale,
) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("auth-two-factor")) }
            @if let Some(message) = error {
                p.form-error id="oauth-challenge-error" role="alert" { (message) }
            }
            @if show_webauthn {
                (super::webauthn::login_prompt(
                    challenge_token,
                    Some((
                        "/oauth/authorize/webauthn/options",
                        "/oauth/authorize/webauthn",
                    )),
                    locale,
                ))
            }
            p.consent-lead {
                (locale.text("auth-code-help"))
            }
            form.auth-form method="post" action="/oauth/authorize" {
                @for (name, value) in hidden {
                    input type="hidden" name=(name) value=(value);
                }
                input type="hidden" name="challenge_token" value=(challenge_token);
                label {
                    (locale.text("auth-code"))
                    input type="text" name="code" inputmode="numeric"
                        autocomplete="one-time-code" autofocus required
                        aria-invalid=[error.map(|_| "true")]
                        aria-describedby=[error.map(|_| "oauth-challenge-error")];
                }
                button type="submit" { (locale.text("auth-verify")) }
            }
        }
    };
    layout::focused_shell_localized(&locale.text("auth-two-factor"), &content, locale)
}

/// The out-of-band success page: shown when a client registered the `oob`
/// redirect URI, so the code must be displayed for the user to copy by hand.
pub(crate) fn oob_code_page(code: &str, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("consent-oob-title")) }
            p { (locale.text("consent-oob-hint")) }
            pre.oob-code { (code) }
        }
    };
    layout::focused_shell_localized(&locale.text("consent-oob-title"), &content, locale)
}

/// The terminal page for denying an out-of-band authorization request. Normal
/// clients receive the standard OAuth error on their registered callback.
pub(crate) fn oauth_denied_page(locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("consent-denied-title")) }
            p { (locale.text("consent-denied-hint")) }
        }
    };
    layout::focused_shell_localized(&locale.text("consent-denied-title"), &content, locale)
}

#[derive(Deserialize)]
pub struct LoginForm {
    /// An e-mail address or a username; the `email` alias keeps older form
    /// posts (and password managers replaying them) working.
    #[serde(alias = "email")]
    identifier: String,
    password: String,
}

/// `POST /login` — verify credentials, mint a session token, set the cookie.
pub async fn login_submit(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let locale = Locale::from_headers(&headers);
    // Reject a cross-site credential submission before any work — login CSRF /
    // session swapping.
    if !crate::auth::same_origin_request(&headers, &state.config.domain) {
        return cross_origin_login_rejection();
    }
    let app = match ensure_web_app(&state).await {
        Ok(app) => app,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await
    {
        return err.into_response();
    }
    // The per-identifier login throttle (Mastodon's `throttle_login_attempts/
    // email`, extended to usernames) — the per-IP leg lives in the rate-limit
    // middleware.
    if let Err(err) = crate::rate_limit::check_email(
        &state,
        crate::rate_limit::Bucket::LoginAttemptsEmail,
        &form.identifier,
    )
    .await
    {
        return err.into_response();
    }

    let ip = remote_ip.map(|ip| ip.to_string());
    let user_agent = crate::auth::user_agent_string(&headers);
    let user = match crate::auth::authenticate_password(
        &state,
        &form.identifier,
        form.password.clone(),
        ip.as_deref(),
        user_agent.as_deref(),
    )
    .await
    {
        Ok(user) => user,
        Err(failure) => {
            let mascot = mascot_url(&state).await;
            let signup_open = signup_open(&state).await;
            return (
                StatusCode::UNAUTHORIZED,
                login_page(
                    &state,
                    mascot.as_deref(),
                    Some(failure.web_message()),
                    signup_open,
                    locale,
                )
                .await,
            )
                .into_response();
        }
    };

    // A 2FA-enabled account has proven its password; hand off to the second
    // factor before minting a session (the sign-in is recorded only once it
    // completes).
    if user.otp_required_for_login {
        let raw = generate_secret();
        if let Err(err) =
            two_factor::create_challenge(&state.pool, &hash_secret(&raw), user.id, "web", None)
                .await
        {
            return crate::error::ApiError::from(err).into_response();
        }
        let show_webauthn = plamenu_db::webauthn_credential::count_by_user(&state.pool, user.id)
            .await
            .unwrap_or(0)
            > 0;
        return challenge_page(&state, &raw, None, show_webauthn, locale)
            .await
            .into_response();
    }

    complete_login(&state, &app, user.id, "password", &headers, remote_ip).await
}

/// Records the sign-in and mints a session token, returning the raw token to
/// install in a cookie — the tail shared by password-only login and a passed
/// second factor.
async fn record_and_mint(
    state: &AppState,
    app: &App,
    user_id: i64,
    method: &str,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Result<String, crate::error::ApiError> {
    // Record the sign-in (Devise trackable + `activity:logins`), seeding the UI
    // locale from Accept-Language on first login. Non-fatal if it fails.
    let locale = crate::auth::accept_language_primary(headers);
    let user_agent = crate::auth::user_agent_string(headers);
    let remote_ip_text = remote_ip.map(|ip| ip.to_string());
    let _ = crate::sign_in::record(
        state,
        user_id,
        locale.as_deref(),
        remote_ip_text.as_deref(),
        user::SignInContext {
            method: Some(method),
            user_agent: user_agent.as_deref(),
        },
    )
    .await;

    mint_session_token(state, app, user_id, headers, remote_ip).await
}

/// Mints the first-party token behind a browser session without recording a
/// second sign-in event. OAuth records its own successful password / second
/// factor method, then uses this tail to keep that browser signed in too.
async fn mint_session_token(
    state: &AppState,
    app: &App,
    user_id: i64,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Result<String, crate::error::ApiError> {
    let raw = generate_secret();
    let user_agent = crate::auth::user_agent_string(headers);
    let remote_ip = remote_ip.map(|ip| ip.to_string());
    oauth::create_token_with_meta(
        &state.pool,
        &hash_secret(&raw),
        app.id,
        Some(user_id),
        WEB_SCOPES,
        oauth::SessionMeta {
            user_agent: user_agent.as_deref(),
            ip: remote_ip.as_deref(),
        },
    )
    .await?;
    Ok(raw)
}

/// Creates and installs a first-party browser session for a user who has just
/// authenticated through OAuth. This lets a restarted authorization request
/// reuse the proven login and the browser's existing multi-account roster.
pub(crate) async fn oauth_session_cookies(
    state: &AppState,
    headers: &HeaderMap,
    user_id: i64,
    remote_ip: Option<IpAddr>,
) -> Result<(String, String), crate::error::ApiError> {
    let app = ensure_web_app(state).await?;
    let raw = mint_session_token(state, &app, user_id, headers, remote_ip).await?;
    Ok(session_cookies(state, &app, headers, user_id, raw).await)
}

/// Completes a form-driven sign-in: a `303` redirect home carrying the session
/// cookie (password-only login and the TOTP challenge).
pub(crate) async fn complete_login(
    state: &AppState,
    app: &App,
    user_id: i64,
    method: &str,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Response {
    let raw = match record_and_mint(state, app, user_id, method, headers, remote_ip).await {
        Ok(raw) => raw,
        Err(err) => return err.into_response(),
    };
    let (active, roster) = session_cookies(state, app, headers, user_id, raw).await;
    (
        StatusCode::SEE_OTHER,
        AppendHeaders([
            (header::LOCATION, "/".to_owned()),
            (header::SET_COOKIE, active),
            (header::SET_COOKIE, roster),
        ]),
    )
        .into_response()
}

/// Completes a fetch-driven sign-in (the `WebAuthn` login ceremony): the session
/// cookie rides a `200` JSON body carrying where the shim should navigate next.
pub(crate) async fn complete_login_json(
    state: &AppState,
    app: &App,
    user_id: i64,
    method: &str,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Response {
    let raw = match record_and_mint(state, app, user_id, method, headers, remote_ip).await {
        Ok(raw) => raw,
        Err(err) => return err.into_response(),
    };
    let (active, roster) = session_cookies(state, app, headers, user_id, raw).await;
    (
        StatusCode::OK,
        AppendHeaders([(header::SET_COOKIE, active), (header::SET_COOKIE, roster)]),
        axum::Json(serde_json::json!({ "redirect": "/" })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ChallengeForm {
    challenge_token: String,
    code: String,
}

/// `POST /login/challenge` — the second-factor step of web sign-in. Bad codes
/// do not feed `record_failed_sign_in`: the challenge's own attempt cap and the
/// per-IP login throttle bound the abuse (matching Mastodon).
pub async fn login_challenge_submit(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    Form(form): Form<ChallengeForm>,
) -> Response {
    let locale = Locale::from_headers(&headers);
    // Bind the second-factor continuation to a same-origin request too, so the
    // 2FA leg cannot be driven cross-site.
    if !crate::auth::same_origin_request(&headers, &state.config.domain) {
        return cross_origin_login_rejection();
    }
    let app = match ensure_web_app(&state).await {
        Ok(app) => app,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await
    {
        return err.into_response();
    }
    match super::two_factor::verify_challenge(&state, &form.challenge_token, "web", &form.code)
        .await
    {
        Ok(verified) => {
            complete_login(&state, &app, verified.user_id, "otp", &headers, remote_ip).await
        }
        Err(super::two_factor::ChallengeError::WrongCode) => {
            let show_webauthn = super::webauthn::challenge_offers_security_key(
                &state,
                &form.challenge_token,
                "web",
            )
            .await;
            let error = locale.text("auth-code-incorrect");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                challenge_page(
                    &state,
                    &form.challenge_token,
                    Some(&error),
                    show_webauthn,
                    locale,
                )
                .await,
            )
                .into_response()
        }
        Err(super::two_factor::ChallengeError::Invalid) => {
            let mascot = mascot_url(&state).await;
            let signup_open = signup_open(&state).await;
            let expired = locale.text("auth-login-expired");
            (
                StatusCode::UNAUTHORIZED,
                login_page(
                    &state,
                    mascot.as_deref(),
                    Some(&expired),
                    signup_open,
                    locale,
                )
                .await,
            )
                .into_response()
        }
        Err(super::two_factor::ChallengeError::Db(err)) => err.into_response(),
    }
}

#[derive(Deserialize)]
pub struct SwitchForm {
    account_id: i64,
    csrf: String,
}

/// `POST /web/accounts/switch` — make one of the browser's other signed-in
/// accounts the active session.
///
/// Possession of the (`HttpOnly`) roster cookie is the credential — it proves the
/// browser already holds the target's token — and a roster-bound CSRF token plus
/// `SameSite=Lax` guard the form. The target token is re-resolved so a revoked or
/// expired entry prunes itself rather than stranding the user on a dead session.
pub async fn switch_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SwitchForm>,
) -> Response {
    let Some(expected) = roster_csrf(&headers) else {
        return Redirect::to("/login").into_response();
    };
    if form.csrf != expected {
        return csrf_rejection();
    }
    let mut roster = read_roster(&headers);
    let Some(entry) = roster
        .iter()
        .find(|e| e.account_id == form.account_id)
        .cloned()
    else {
        return Redirect::to("/login?switch=1").into_response();
    };
    if user_for_token(&state, &entry.token).await.is_err() {
        // Stale entry — drop it and send the user back to the chooser.
        roster.retain(|e| e.account_id != form.account_id);
        let roster_cookie = if roster.is_empty() {
            clear_roster_cookie()
        } else {
            set_roster_cookie(&roster)
        };
        return (
            StatusCode::SEE_OTHER,
            AppendHeaders([
                (header::LOCATION, "/login?switch=1".to_owned()),
                (header::SET_COOKIE, roster_cookie),
            ]),
        )
            .into_response();
    }
    (
        StatusCode::SEE_OTHER,
        AppendHeaders([
            (header::LOCATION, "/".to_owned()),
            (header::SET_COOKIE, set_cookie(&entry.token)),
        ]),
    )
        .into_response()
}

/// `POST /logout` body — the shell's logout button submits the session-bound
/// CSRF token like every other signed-in mutation. `default` so a session-less
/// POST (nothing to protect) still parses.
#[derive(Deserialize)]
pub struct LogoutForm {
    #[serde(default)]
    csrf: String,
}

/// `POST /logout` — sign the active account out: revoke its token, drop it from
/// the roster and clear the session cookie. Any other signed-in accounts are
/// kept so the `/login` chooser can offer to switch straight into them.
///
/// The revocation is load-bearing, not best-effort: the browser
/// is about to discard its only copy of the token, so if the server-side
/// revocation cannot be confirmed the handler refuses to clear anything and
/// answers `503` — the cookie (and the token it names) stays in the browser,
/// making a simple resubmit of the logout form the retry mechanism. A durable
/// server-side retry queue cannot exist here: the failure mode is the database
/// being unreachable, which is exactly where a Postgres-backed queue could not
/// record the debt either.
///
/// A signed-in logout is a state-changing form like any other, so it verifies
/// the session-bound CSRF token the shell's logout button already submits —
/// `SameSite=Lax` alone would leave a forged same-site POST able to sign the
/// victim out (an unpromoted lead closed 2026-07-30).
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<LogoutForm>, axum::extract::rejection::FormRejection>,
) -> Response {
    let active = cookie_value(&headers, COOKIE_NAME).map(str::to_owned);
    if let Some(raw) = &active {
        // `Result` so a body-less POST still reaches the handler; it then
        // fails this comparison rather than 415ing before the check.
        if form.map(|Form(f)| f.csrf).unwrap_or_default() != csrf_token(raw) {
            return csrf_rejection();
        }
        let revoked = match ensure_web_app(&state).await {
            Ok(app) => oauth::revoke_token(&state.pool, &hash_secret(raw), app.id)
                .await
                .map_err(|error| {
                    tracing::error!(%error, "logout: token revocation failed; token remains live");
                })
                .is_ok(),
            Err(error) => {
                tracing::error!(%error, "logout: could not resolve web app to revoke token");
                false
            }
        };
        if !revoked {
            // Keep every cookie: reporting success here would strand a live
            // credential the browser no longer holds a copy of.
            return crate::error::ApiError::ServiceUnavailable(
                "Logout could not be completed; please try again.".to_owned(),
            )
            .into_response();
        }
    }
    let mut roster = read_roster(&headers);
    if let Some(raw) = &active {
        roster.retain(|e| &e.token != raw);
    }
    let roster_cookie = if roster.is_empty() {
        clear_roster_cookie()
    } else {
        set_roster_cookie(&roster)
    };
    (
        StatusCode::SEE_OTHER,
        AppendHeaders([
            (header::LOCATION, "/login".to_owned()),
            (header::SET_COOKIE, clear_cookie()),
            (header::SET_COOKIE, roster_cookie),
        ]),
    )
        .into_response()
}
