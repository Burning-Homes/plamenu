//! `OAuth2`: authorization-code flow with PKCE (S256), plus `client_credentials`.
//!
//! The `/oauth/authorize` page is the one place a headless server must render
//! HTML: clients open it in a browser, choose or sign into an account, approve
//! access, and are redirected back with a code. A successful credential flow
//! also creates a first-party browser session so a restarted request can reuse
//! that login (including the browser's multi-account roster).

use std::net::IpAddr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use plamenu_db::oauth::{self, App, NewGrant};
// The authorization request a second-factor challenge is bound to lives beside
// its storage: the password leg persists it on the
// `two_factor_challenges` row, and the TOTP / WebAuthn legs mint the grant
// from THOSE server-held values, never from the resubmitted form. `scope`
// holds the canonicalized *granted* scope set.
use plamenu_db::two_factor::OauthChallengeRequest;
use plamenu_db::{two_factor, user, webauthn_credential};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use webauthn_rs::prelude::{PublicKeyCredential, RequestChallengeResponse};

use super::params::parse_body;
use crate::auth::{generate_secret, hash_secret, pkce_s256, secret_hash_eq};
use crate::error::ApiError;
use crate::instance_policy::RemoteIp;
use crate::state::AppState;
use crate::web::i18n::Locale;
use crate::web::session;
use crate::web::two_factor::{ChallengeError, verify_challenge};

pub const OOB_REDIRECT: &str = "urn:ietf:wg:oauth:2.0:oob";

/// The most live app-level (`client_credentials`) tokens one app may hold at
/// once. Generous for legitimate service integrations (which need a small
/// handful, re-minted occasionally), but a hard ceiling so an anonymous caller
/// that registered one app cannot grow the token table without bound.
const APP_TOKEN_CAP: i64 = 10;

/// The scopes an authorization request may actually be granted.
///
/// Canonicalized through the same parser app registration uses (so an unknown
/// or duplicated token cannot be persisted on a grant), then held to what the
/// client actually registered for under the shared lattice — RFC 6749
/// §4.1.2.1's `invalid_scope`: a client may not obtain, and a resource owner
/// may not be asked to approve, access wider than the application declared.
/// Without this the `authorization_code` path took the query string verbatim,
/// so an app registered for `read` could mint itself `write`/`admin:write`
/// (the `client_credentials` path was already bounded).
///
/// An omitted scope keeps Doorkeeper's `read` default, validated the same way.
fn granted_scopes(app: &App, requested: Option<&str>) -> Result<String, ApiError> {
    let raw = requested
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .unwrap_or("read");
    let normalized = crate::oauth_app::normalize_scopes(raw)
        .map_err(|_| ApiError::BadRequest("invalid_scope".into()))?;
    if !normalized
        .split_whitespace()
        .all(|scope| crate::auth::scopes_satisfy(&app.scopes, scope))
    {
        return Err(ApiError::BadRequest("invalid_scope".into()));
    }
    Ok(normalized)
}

async fn known_app(state: &AppState, client_id: &str, redirect_uri: &str) -> Result<App, ApiError> {
    let app = oauth::find_app_by_client_id(&state.pool, client_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("unknown client_id".into()))?;
    if !app.redirect_uris.iter().any(|u| u == redirect_uri) {
        return Err(ApiError::BadRequest(
            "redirect_uri is not registered".into(),
        ));
    }
    Ok(app)
}

#[derive(Deserialize)]
pub struct AuthorizeParams {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

/// RFC 7636's shared syntax for `code_challenge` and `code_verifier`:
/// 43–128 characters of the `unreserved` set (`ALPHA / DIGIT / "-" / "." /
/// "_" / "~"`). Enforced on every entry path so a grant can
/// never carry a challenge no legal verifier could match, and a verifier is
/// bounded before it is hashed.
fn valid_pkce_syntax(value: &str) -> bool {
    (43..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

fn validate_pkce_challenge(challenge: Option<&str>) -> Result<(), ApiError> {
    match challenge {
        Some(challenge) if !valid_pkce_syntax(challenge) => Err(ApiError::BadRequest(
            "code_challenge must be 43-128 unreserved characters".into(),
        )),
        _ => Ok(()),
    }
}

fn validate_authorize(params: &AuthorizeParams) -> Result<(), ApiError> {
    if params.response_type != "code" {
        return Err(ApiError::BadRequest("response_type must be 'code'".into()));
    }
    // PKCE is optional (older Mastodon apps don't send it) but when used,
    // only S256 is accepted — same policy as Mastodon 4.3+.
    if params.code_challenge.is_some() && params.code_challenge_method.as_deref() != Some("S256") {
        return Err(ApiError::BadRequest(
            "code_challenge_method must be S256".into(),
        ));
    }
    validate_pkce_challenge(params.code_challenge.as_deref())
}

/// `GET /oauth/authorize` — the sign-in / consent form.
pub async fn authorize_form(
    State(state): State<AppState>,
    Extension(security): Extension<crate::RequestSecurityContext>,
    session::MaybeWebUser(web_session): session::MaybeWebUser,
    locale: Locale,
    headers: HeaderMap,
    Query(params): Query<AuthorizeParams>,
) -> Result<Response, ApiError> {
    let app = known_app(&state, &params.client_id, &params.redirect_uri).await?;
    validate_authorize(&params)?;
    allow_callback_form_action(&security, &params.redirect_uri);

    // Refuse an over-broad or unknown scope here rather than rendering a
    // consent screen for access the grant would go on to refuse. The submit
    // handler re-checks: these hidden fields are client-controlled.
    let scopes = granted_scopes(&app, params.scope.as_deref())?;
    let scopes = scopes.as_str();
    // The authorization-request parameters that must round-trip into the POST
    // so the submit handler can re-validate and mint the grant.
    let mut hidden = vec![
        ("client_id", params.client_id.as_str()),
        ("redirect_uri", params.redirect_uri.as_str()),
        ("scope", scopes),
    ];
    if let Some(oauth_state) = &params.state {
        hidden.push(("state", oauth_state));
    }
    if let Some(challenge) = &params.code_challenge {
        hidden.push(("code_challenge", challenge));
    }

    Ok(session::consent_page(
        &state,
        &headers,
        web_session.as_ref(),
        &app.name,
        scopes,
        &hidden,
        locale,
    )
    .await
    .into_response())
}

#[derive(Deserialize)]
pub struct AuthorizeSubmit {
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    // A signed-in consent screen posts an explicit decision. `switch` selects
    // a browser-rostered account and then returns to the same consent request.
    pub decision: Option<String>,
    pub csrf: Option<String>,
    pub roster_csrf: Option<String>,
    pub account_id: Option<i64>,
    // Credentials on the first (password) submission; absent on the
    // second-factor submission, which instead carries `challenge_token`+`code`.
    // `identifier` is an e-mail address or a username; the `email` alias keeps
    // older form posts working.
    #[serde(alias = "email")]
    pub identifier: Option<String>,
    pub password: Option<String>,
    pub challenge_token: Option<String>,
    pub code: Option<String>,
}

/// The authorization-request parameters that must round-trip into the POST,
/// as hidden form fields (shared by the consent page and the 2FA prompt).
fn hidden_params(form: &AuthorizeSubmit) -> Vec<(&str, &str)> {
    let mut hidden = vec![
        ("client_id", form.client_id.as_str()),
        ("redirect_uri", form.redirect_uri.as_str()),
        ("scope", form.scope.as_deref().unwrap_or("read")),
    ];
    if let Some(oauth_state) = &form.state {
        hidden.push(("state", oauth_state));
    }
    if let Some(challenge) = &form.code_challenge {
        hidden.push(("code_challenge", challenge));
    }
    hidden
}

/// Re-validates the client-controlled POST fields and freezes their canonical
/// form for every path that can ultimately mint a grant.
fn submitted_request(app: &App, form: &AuthorizeSubmit) -> Result<OauthChallengeRequest, ApiError> {
    let scope = granted_scopes(app, form.scope.as_deref())?;
    validate_pkce_challenge(form.code_challenge.as_deref())?;
    Ok(OauthChallengeRequest {
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        scope,
        state: form.state.clone(),
        code_challenge: form.code_challenge.clone(),
    })
}

/// Rebuilds the validated authorization request as an internal GET. Account
/// selection redirects here so the next page is rendered under the chosen
/// active session and presents the allow / deny decision.
fn authorization_path(request: &OauthChallengeRequest) -> Result<String, ApiError> {
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", request.client_id.as_str()),
        ("redirect_uri", request.redirect_uri.as_str()),
        ("scope", request.scope.as_str()),
    ];
    if let Some(oauth_state) = &request.state {
        params.push(("state", oauth_state));
    }
    if let Some(challenge) = &request.code_challenge {
        params.push(("code_challenge", challenge));
        params.push(("code_challenge_method", "S256"));
    }
    let query =
        serde_urlencoded::to_string(params).map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(format!("/oauth/authorize?{query}"))
}

/// Standard OAuth denial: send an error to a normal registered callback, or
/// show a terminal page for the out-of-band callback convention.
fn deny_authorization(
    request: &OauthChallengeRequest,
    locale: Locale,
) -> Result<Response, ApiError> {
    if request.redirect_uri == OOB_REDIRECT {
        return Ok(session::oauth_denied_page(locale).into_response());
    }
    let mut params = vec![("error", "access_denied")];
    if let Some(oauth_state) = &request.state {
        params.push(("state", oauth_state));
    }
    let query =
        serde_urlencoded::to_string(params).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let separator = if request.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    Ok((
        StatusCode::FOUND,
        [(
            header::LOCATION,
            format!("{}{separator}{query}", request.redirect_uri),
        )],
    )
        .into_response())
}

fn append_cookie(response: &mut Response, cookie: &str) -> Result<(), ApiError> {
    let value =
        HeaderValue::from_str(cookie).map_err(|error| ApiError::Internal(Box::new(error)))?;
    response.headers_mut().append(header::SET_COOKIE, value);
    Ok(())
}

/// Handles the three actions that exist only on a reusable browser-session
/// consent screen. `None` means this is the credential / second-factor form
/// and the caller should continue through the ordinary authentication path.
async fn browser_decision(
    state: &AppState,
    app: &App,
    request: &OauthChallengeRequest,
    form: &AuthorizeSubmit,
    web_session: Option<&session::WebUser>,
    headers: &HeaderMap,
) -> Result<Option<Response>, ApiError> {
    match form.decision.as_deref() {
        Some("allow" | "deny") => {
            let web_user = web_session
                .ok_or_else(|| ApiError::Unauthorized("No signed-in account found".into()))?;
            if !form
                .csrf
                .as_deref()
                .is_some_and(|submitted| web_user.csrf_ok(submitted))
            {
                return Err(ApiError::Forbidden("invalid CSRF token".into()));
            }
            if form.decision.as_deref() == Some("deny") {
                return deny_authorization(request, web_user.locale).map(Some);
            }
            mint_grant(
                state,
                app,
                web_user.current.user.id,
                request,
                web_user.locale,
            )
            .await
            .map(Some)
        }
        Some("switch") => {
            let account_id = form
                .account_id
                .ok_or_else(|| ApiError::BadRequest("account_id is required".into()))?;
            let submitted_csrf = form
                .roster_csrf
                .as_deref()
                .ok_or_else(|| ApiError::Forbidden("invalid CSRF token".into()))?;
            let active =
                session::oauth_switch_cookie(state, headers, account_id, submitted_csrf).await?;
            let location = authorization_path(request)?;
            let mut response =
                (StatusCode::SEE_OTHER, [(header::LOCATION, location)]).into_response();
            append_cookie(&mut response, &active)?;
            Ok(Some(response))
        }
        Some(_) => Err(ApiError::BadRequest(
            "invalid authorization decision".into(),
        )),
        None => Ok(None),
    }
}

/// `POST /oauth/authorize` — verify credentials (and any second factor), then
/// mint a single-use code.
pub async fn authorize_submit(
    State(state): State<AppState>,
    Extension(security): Extension<crate::RequestSecurityContext>,
    RemoteIp(remote_ip): RemoteIp,
    session::MaybeWebUser(web_session): session::MaybeWebUser,
    locale: Locale,
    headers: HeaderMap,
    Form(form): Form<AuthorizeSubmit>,
) -> Result<Response, ApiError> {
    // The consent page collects a username and password sessionlessly, so its
    // submit is a pre-session credential POST like `/login`: refuse a cross-site
    // request (login CSRF / session swapping) before any work. This covers both
    // the password and second-factor legs, which share this handler (QC #62).
    if !crate::auth::same_origin_request(&headers, &state.config.domain) {
        return Err(ApiError::Forbidden(
            "This sign-in request did not come from this site.".into(),
        ));
    }
    let app = known_app(&state, &form.client_id, &form.redirect_uri).await?;
    // The hidden fields are client-controlled, so the full request is
    // validated again here — scope bound to the app's registration and PKCE
    // syntax checked — before a password is spent or a second factor is
    // demanded for an authorization that could never be granted.
    let request = submitted_request(&app, &form)?;
    allow_callback_form_action(&security, &request.redirect_uri);
    crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await?;

    // A reusable browser session gets a true consent decision without another
    // password. Both allow and deny are session-CSRF protected. Switching uses
    // the roster-bound token because it also works when the active cookie has
    // expired but another rostered account is still live.
    if let Some(response) = browser_decision(
        &state,
        &app,
        &request,
        &form,
        web_session.as_ref(),
        &headers,
    )
    .await?
    {
        return Ok(response);
    }

    // Second-factor submission: the consent form came back with a challenge
    // token and a code instead of credentials. The grant is minted from the
    // authorization request stored on the challenge row, not from this form.
    if let (Some(token), Some(code)) = (form.challenge_token.clone(), form.code.clone()) {
        return complete_challenge(&state, &form, &headers, remote_ip, &token, &code).await;
    }

    // Password submission.
    let (Some(identifier), Some(password)) = (form.identifier.as_deref(), form.password.as_deref())
    else {
        return Err(ApiError::BadRequest("credentials are required".into()));
    };
    // Mastodon's `throttle_login_attempts/email` (the IP leg is the
    // rate-limit middleware's job), keyed by whichever identifier was typed.
    crate::rate_limit::check_email(
        &state,
        crate::rate_limit::Bucket::LoginAttemptsEmail,
        identifier,
    )
    .await?;

    let Some(user) = crate::auth::find_user_by_login_identifier(&state, identifier).await? else {
        // Spend the same classes of work as the wrong-password branch below —
        // locked-check read, Argon2, failure-recording writes — so a missing
        // identifier is not timing-distinguishable, off the async threads
        // behind the crypto gate.
        crate::auth::equalized_failed_login(&state, password.to_owned()).await;
        return Err(ApiError::Unauthorized(
            "invalid email/username or password".into(),
        ));
    };
    if user::login_locked(&state.pool, user.id).await? {
        return Err(ApiError::Forbidden(
            "Your login is temporarily locked. Try again later.".into(),
        ));
    }
    if !crate::auth::verify_password_gated(password.to_owned(), user.password_hash.clone()).await {
        let locked = user::record_failed_sign_in(&state.pool, user.id).await?;
        return Err(if locked {
            ApiError::Forbidden("Your login is temporarily locked. Try again later.".into())
        } else {
            ApiError::Unauthorized("invalid email/username or password".into())
        });
    }
    // A non-functional login cannot sign in (Mastodon blocks at Devise
    // authentication) — the `require_user!` ladder's order and wording.
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
    if let Some(email) = user.email.as_deref() {
        crate::instance_policy::ensure_email_login_allowed(&state.pool, email).await?;
    }

    // A 2FA-enabled account has proven its password; defer the sign-in and
    // grant to the second factor. The prompt offers TOTP always, and a security
    // key when one is registered (verified sessionlessly against the challenge
    // token, exactly as the first-party web sign-in does). The authorization
    // request the password just approved is bound to the challenge row, and it
    // is THOSE values the second-factor leg will mint from — a substituted
    // form on the second leg changes nothing.
    if user.otp_required_for_login {
        let raw = generate_secret();
        two_factor::create_challenge(
            &state.pool,
            &hash_secret(&raw),
            user.id,
            "oauth",
            Some(&request),
        )
        .await?;
        let show_webauthn = webauthn_credential::count_by_user(&state.pool, user.id)
            .await
            .unwrap_or(0)
            > 0;
        return Ok(session::oauth_challenge_page(
            &hidden_params(&form),
            &raw,
            None,
            show_webauthn,
            locale,
        )
        .into_response());
    }

    authenticate_and_mint(
        &state, &app, user.id, &request, "password", &headers, remote_ip,
    )
    .await
}

/// Adds only the registered callback's origin (or native-app scheme) to the
/// authorization document's form target policy. Chromium carries
/// `form-action` across redirects after a POST; without this exception it
/// blocks the ordinary OAuth redirect even though the form itself posts to
/// `/oauth/authorize` on this origin.
fn allow_callback_form_action(security: &crate::RequestSecurityContext, redirect_uri: &str) {
    if redirect_uri == OOB_REDIRECT {
        return;
    }
    let Ok(parsed) = url::Url::parse(redirect_uri) else {
        return;
    };
    let source = if parsed.host_str().is_some() {
        parsed.origin().ascii_serialization()
    } else {
        // A URI scheme is a valid CSP source expression (`myapp:`) and is the
        // narrowest representable allowance for native-app callbacks.
        format!("{}:", parsed.scheme())
    };
    security.allow_oauth_form_action(source);
}

/// The second-factor leg of the authorize flow: verify the code, then record
/// the deferred sign-in and mint the grant — from the authorization request
/// stored on the challenge row at password time, never from the resubmitted
/// form, whose values a captured token could otherwise swap.
async fn complete_challenge(
    state: &AppState,
    form: &AuthorizeSubmit,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
    token: &str,
    code: &str,
) -> Result<Response, ApiError> {
    // The second-factor leg re-renders the prompt and can reject it, so it
    // needs the locale too; it is still sessionless, so the headers decide.
    let locale = Locale::from_headers(headers);
    match verify_challenge(state, token, "oauth", code).await {
        Ok(verified) => {
            let (app, request) = bound_request(state, verified.oauth_request, locale).await?;
            authenticate_and_mint(
                state,
                &app,
                verified.user_id,
                &request,
                "otp",
                headers,
                remote_ip,
            )
            .await
        }
        Err(ChallengeError::WrongCode) => {
            let show_webauthn =
                crate::web::webauthn::challenge_offers_security_key(state, token, "oauth").await;
            Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                session::oauth_challenge_page(
                    &hidden_params(form),
                    token,
                    Some(&locale.text("auth-code-incorrect")),
                    show_webauthn,
                    locale,
                ),
            )
                .into_response())
        }
        Err(ChallengeError::Invalid) => {
            Err(ApiError::Unauthorized(locale.text("webauthn-expired")))
        }
        Err(ChallengeError::Db(err)) => Err(err),
    }
}

/// Records the successful sign-in (Devise trackable + `activity:logins`),
/// seeding the UI locale from Accept-Language on first login.
async fn record_sign_in(
    state: &AppState,
    user_id: i64,
    method: &str,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Result<(), ApiError> {
    let locale = crate::auth::accept_language_primary(headers);
    let user_agent = crate::auth::user_agent_string(headers);
    let remote_ip = remote_ip.map(|ip| ip.to_string());
    crate::sign_in::record(
        state,
        user_id,
        locale.as_deref(),
        remote_ip.as_deref(),
        user::SignInContext {
            method: Some(method),
            user_agent: user_agent.as_deref(),
        },
    )
    .await?;
    Ok(())
}

/// Completes a credential-backed authorization and also keeps the browser
/// signed into Plamenu. The OAuth app still receives its ordinary grant; the
/// separate first-party token is only installed in the secure browser cookies
/// and feeds later consent/account-choice requests.
async fn authenticate_and_mint(
    state: &AppState,
    app: &App,
    user_id: i64,
    request: &OauthChallengeRequest,
    method: &str,
    headers: &HeaderMap,
    remote_ip: Option<IpAddr>,
) -> Result<Response, ApiError> {
    record_sign_in(state, user_id, method, headers, remote_ip).await?;
    let (active, roster) =
        session::oauth_session_cookies(state, headers, user_id, remote_ip).await?;
    let locale = Locale::from_headers(headers);
    let mut response = mint_grant(state, app, user_id, request, locale).await?;
    append_cookie(&mut response, &active)?;
    append_cookie(&mut response, &roster)?;
    Ok(response)
}

/// Recovers the authorization request a second-factor challenge was bound to
/// and re-resolves its app. A challenge with no stored request cannot mint
/// anything — the user restarts the flow (only an `"oauth"`-context challenge
/// minted before this binding existed could hit that, and those expire within
/// minutes). The app is looked up afresh so a client deleted (or a callback
/// deregistered) between the two legs is refused.
async fn bound_request(
    state: &AppState,
    stored: Option<OauthChallengeRequest>,
    locale: Locale,
) -> Result<(App, OauthChallengeRequest), ApiError> {
    let request = stored.ok_or_else(|| ApiError::Unauthorized(locale.text("webauthn-expired")))?;
    let app = known_app(state, &request.client_id, &request.redirect_uri).await?;
    Ok((app, request))
}

/// The result of minting an authorization code: where to send the client.
enum GrantOutcome {
    /// Redirect back to the app's `redirect_uri` with the code (and `state`).
    Redirect(String),
    /// The out-of-band code to display for manual copy.
    Oob(String),
}

/// Creates the single-use authorization code and works out where the client goes
/// next, without committing to a response shape — the form flow renders a `302`
/// / OOB page, the `WebAuthn` flow renders JSON the shim navigates from.
async fn create_grant_outcome(
    state: &AppState,
    app: &App,
    user_id: i64,
    request: &OauthChallengeRequest,
) -> Result<GrantOutcome, ApiError> {
    // The authoritative scope bound: every path that mints a grant (password,
    // TOTP, WebAuthn) funnels through here, so the stored grant — and the
    // token it is exchanged for — can never exceed the app's registration,
    // even if that registration narrowed after the request was bound.
    let scopes = granted_scopes(app, Some(&request.scope))?;
    let code = generate_secret();
    oauth::create_grant(
        &state.pool,
        NewGrant {
            code_hash: &hash_secret(&code),
            app_id: app.id,
            user_id,
            redirect_uri: &request.redirect_uri,
            scopes: &scopes,
            pkce_challenge: request.code_challenge.as_deref(),
        },
    )
    .await?;

    if request.redirect_uri == OOB_REDIRECT {
        return Ok(GrantOutcome::Oob(code));
    }
    let mut location = format!(
        "{}{}code={code}",
        request.redirect_uri,
        if request.redirect_uri.contains('?') {
            "&"
        } else {
            "?"
        },
    );
    if let Some(oauth_state) = &request.state {
        let encoded = serde_urlencoded::to_string([("state", oauth_state)])
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        location.push('&');
        location.push_str(&encoded);
    }
    Ok(GrantOutcome::Redirect(location))
}

/// Mints the code and returns the redirect back to the client (or the
/// out-of-band code page) — the form (password / TOTP) submission flow.
async fn mint_grant(
    state: &AppState,
    app: &App,
    user_id: i64,
    request: &OauthChallengeRequest,
    locale: Locale,
) -> Result<Response, ApiError> {
    Ok(
        match create_grant_outcome(state, app, user_id, request).await? {
            GrantOutcome::Oob(code) => session::oob_code_page(&code, locale).into_response(),
            GrantOutcome::Redirect(location) => {
                (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
            }
        },
    )
}

#[derive(Deserialize)]
pub struct AuthorizeWebauthnOptions {
    challenge_token: String,
}

#[derive(Serialize)]
struct AuthorizeWebauthnOptionsResponse {
    options: RequestChallengeResponse,
}

/// `POST /oauth/authorize/webauthn/options` — begins the security-key assertion
/// for the account behind a live `"oauth"` 2FA challenge. Sessionless: the
/// challenge token is the secret, shared with the first-party sign-in helper.
pub async fn authorize_webauthn_options(
    State(state): State<AppState>,
    locale: Locale,
    Json(req): Json<AuthorizeWebauthnOptions>,
) -> Result<Response, ApiError> {
    let options =
        crate::web::webauthn::begin_assertion(&state, &req.challenge_token, "oauth", locale)
            .await?;
    Ok(Json(AuthorizeWebauthnOptionsResponse { options }).into_response())
}

#[derive(Deserialize)]
pub struct AuthorizeWebauthnFinish {
    challenge_token: String,
    credential: PublicKeyCredential,
    // Older shims also post the authorization-request parameters here; they are
    // deliberately ignored — the grant is minted from the request bound to the
    // challenge row at password time.
}

/// `POST /oauth/authorize/webauthn` — finishes the security-key assertion and,
/// on success, records the sign-in and mints the grant — from the
/// authorization request stored on the challenge row, never from
/// client-supplied parameters. Returns JSON the shim navigates
/// from: `{ "redirect": <url> }`, or `{ "oob_code": <code> }` for the
/// out-of-band redirect URI.
pub async fn authorize_webauthn_finish(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    locale: Locale,
    headers: HeaderMap,
    Json(req): Json<AuthorizeWebauthnFinish>,
) -> Result<Response, ApiError> {
    crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await?;

    let verified = crate::web::webauthn::verify_assertion(
        &state,
        &req.challenge_token,
        "oauth",
        &req.credential,
        locale,
    )
    .await?;
    let (app, request) = bound_request(&state, verified.oauth_request, locale).await?;
    record_sign_in(&state, verified.user_id, "webauthn", &headers, remote_ip).await?;
    let (active, roster) =
        session::oauth_session_cookies(&state, &headers, verified.user_id, remote_ip).await?;

    let mut response = match create_grant_outcome(&state, &app, verified.user_id, &request).await? {
        GrantOutcome::Redirect(location) => Json(json!({ "redirect": location })).into_response(),
        GrantOutcome::Oob(code) => Json(json!({ "oob_code": code })).into_response(),
    };
    append_cookie(&mut response, &active)?;
    append_cookie(&mut response, &roster)?;
    Ok(response)
}

#[derive(Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub code_verifier: Option<String>,
    pub scope: Option<String>,
}

/// Authenticates the client by id + secret (POST-body style, what Mastodon
/// apps use).
async fn authenticated_app(state: &AppState, request: &TokenRequest) -> Result<App, ApiError> {
    let client_id = request
        .client_id
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("invalid_client".into()))?;
    let client_secret = request
        .client_secret
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("invalid_client".into()))?;
    let app = oauth::find_app_by_client_id(&state.pool, client_id)
        .await?
        .ok_or_else(|| ApiError::Unauthorized("invalid_client".into()))?;
    if !secret_hash_eq(&hash_secret(client_secret), &app.client_secret_hash) {
        return Err(ApiError::Unauthorized("invalid_client".into()));
    }
    Ok(app)
}

/// `POST /oauth/token`.
pub async fn token(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::Json<Value>, ApiError> {
    let request: TokenRequest = parse_body(&headers, &body)?;
    let app = authenticated_app(&state, &request).await?;
    crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await?;

    let (user_id, scopes) = match request.grant_type.as_str() {
        "authorization_code" => {
            let code = request
                .code
                .as_deref()
                .ok_or_else(|| ApiError::BadRequest("invalid_grant".into()))?;
            let grant = oauth::take_grant(&state.pool, &hash_secret(code))
                .await?
                .ok_or_else(|| ApiError::BadRequest("invalid_grant".into()))?;
            if grant.app_id != app.id
                || request.redirect_uri.as_deref() != Some(grant.redirect_uri.as_str())
            {
                return Err(ApiError::BadRequest("invalid_grant".into()));
            }
            if let Some(challenge) = &grant.pkce_challenge {
                // RFC 7636 §4.1: the verifier has the same 43–128-char
                // unreserved syntax as the challenge — enforced before it is
                // hashed.
                let verified = request.code_verifier.as_deref().is_some_and(|verifier| {
                    valid_pkce_syntax(verifier) && &pkce_s256(verifier) == challenge
                });
                if !verified {
                    return Err(ApiError::BadRequest("invalid_grant".into()));
                }
            }
            let user = user::find_by_id(&state.pool, grant.user_id)
                .await?
                .ok_or_else(|| ApiError::BadRequest("invalid_grant".into()))?;
            if let Some(email) = user.email.as_deref() {
                crate::instance_policy::ensure_email_login_allowed(&state.pool, email).await?;
            }
            (Some(grant.user_id), grant.scopes)
        }
        "client_credentials" => {
            // Bound and canonicalize the requested scope through the same parser
            // the app-registration endpoint uses, so an anonymous caller cannot
            // persist a near-body-limit `scopes` value on every request (QC
            // audit #64). Absent a scope, fall back to the app's own registered
            // (already-normalized) scopes.
            let scopes = match request.scope.as_deref() {
                Some(raw) => {
                    let normalized = crate::oauth_app::normalize_scopes(raw)
                        .map_err(|_| ApiError::BadRequest("invalid_scope".into()))?;
                    // A client may not mint itself broader access than it
                    // registered for; every requested scope must be covered by
                    // the app's granted scopes under the shared lattice.
                    if !normalized
                        .split_whitespace()
                        .all(|scope| crate::auth::scopes_satisfy(&app.scopes, scope))
                    {
                        return Err(ApiError::BadRequest("invalid_scope".into()));
                    }
                    normalized
                }
                None => app.scopes.clone(),
            };
            (None, scopes)
        }
        _ => return Err(ApiError::BadRequest("unsupported_grant_type".into())),
    };

    let access_token = generate_secret();
    let user_agent = crate::auth::user_agent_string(&headers);
    let ip = remote_ip.map(|ip| ip.to_string());
    let meta = oauth::SessionMeta {
        user_agent: user_agent.as_deref(),
        ip: ip.as_deref(),
    };
    // App-level `client_credentials` tokens (no resource owner) are minted
    // through the capped path so one app can hold only a bounded number of
    // them, however fast an anonymous caller asks. User tokens
    // from `authorization_code` belong to a real account and keep the ordinary
    // path.
    let stored = match user_id {
        Some(user_id) => {
            oauth::create_token_with_meta(
                &state.pool,
                &hash_secret(&access_token),
                app.id,
                Some(user_id),
                &scopes,
                meta,
            )
            .await?
        }
        None => {
            oauth::create_app_token_capped(
                &state.pool,
                &hash_secret(&access_token),
                app.id,
                &scopes,
                meta,
                APP_TOKEN_CAP,
            )
            .await?
        }
    };
    Ok(axum::Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "scope": stored.scopes,
        "created_at": stored.created_at.unix_timestamp(),
    })))
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token: Option<String>,
}

/// `POST /oauth/revoke` (RFC 7009: unknown tokens still return 200).
pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::Json<Value>, ApiError> {
    let request: RevokeRequest = parse_body(&headers, &body)?;
    let app = authenticated_app(
        &state,
        &TokenRequest {
            grant_type: String::new(),
            code: None,
            client_id: request.client_id.clone(),
            client_secret: request.client_secret.clone(),
            redirect_uri: None,
            code_verifier: None,
            scope: None,
        },
    )
    .await?;
    if let Some(bearer) = request.token.as_deref() {
        oauth::revoke_token(&state.pool, &hash_secret(bearer), app.id).await?;
    }
    Ok(axum::Json(json!({})))
}
