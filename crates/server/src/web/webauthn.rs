//! `WebAuthn` security keys as a second factor.
//!
//! Two ceremonies, both driven by the JS shim in the web assets (there is no
//! no-JS path for `WebAuthn` itself — the TOTP form is always the fallback):
//!
//! * **Registration** (settings, requires TOTP already on): the browser asks
//!   `/web/settings/webauthn/options` for a challenge, runs `navigator.credentials.create`, and
//!   posts the attestation back to `/web/settings/webauthn`. Removal is a plain CSRF-guarded form.
//! * **Login** (after the password step of a 2FA account): the challenge page asks
//!   `/login/webauthn/options`, runs `navigator.credentials.get`, and posts the assertion to
//!   `/login/webauthn`, which sets the session cookie.
//!
//! In-flight ceremony state lives in a `two_factor_challenges` row — the same
//! table the TOTP challenge uses — because the login flow is sessionless and
//! there is nowhere else to keep it. Registration rows use
//! `context = "webauthn_reg"`; login auth state is written onto the live
//! `context = "web"` challenge created by the password step.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::webauthn_credential::{self, WebauthnCredential};
use plamenu_db::{DbError, two_factor};
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::{
    CreationChallengeResponse, CredentialID, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, SecurityKey, SecurityKeyAuthentication, SecurityKeyRegistration,
    Uuid,
};

use super::i18n::Locale;
use super::session::{WebUser, complete_login_json, csrf_rejection, ensure_web_app};
use super::settings::{field, form_pairs, redirect_to};
use crate::auth::{generate_secret, hash_secret};
use crate::error::ApiError;
use crate::instance_policy::RemoteIp;
use crate::state::AppState;

/// The longest a key nickname may be — long enough to be descriptive, short
/// enough to render tidily in the settings list.
const MAX_NICKNAME_LEN: usize = 60;

const SETTINGS_PATH: &str = "/settings/security";

// ---- base64url helpers -------------------------------------------------

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

/// A fresh, opaque per-user `WebAuthn` handle (16 random bytes shaped as a UUID).
fn new_handle() -> Uuid {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness available");
    Uuid::from_bytes(bytes)
}

// ---- Shared -------------------------------------------------------------

/// Whether the account behind a live login challenge (of the given `context`)
/// has any security key — drives whether the challenge page offers the
/// `WebAuthn` path. Shared by the web (`"web"`) and OAuth (`"oauth"`) 2FA
/// prompts.
pub(crate) async fn challenge_offers_security_key(
    state: &AppState,
    raw_token: &str,
    context: &str,
) -> bool {
    let Ok(Some(challenge)) =
        two_factor::find_challenge(&state.pool, &hash_secret(raw_token), context).await
    else {
        return false;
    };
    webauthn_credential::count_by_user(&state.pool, challenge.user_id)
        .await
        .unwrap_or(0)
        > 0
}

/// Loads a user's stored keys, paired with their deserialized `SecurityKey`
/// (the crypto material). A credential that fails to deserialize is a corrupt
/// row and surfaces as an internal error rather than being silently skipped.
async fn load_keys(
    state: &AppState,
    user_id: i64,
) -> Result<Vec<(WebauthnCredential, SecurityKey)>, ApiError> {
    let rows = webauthn_credential::list_by_user(&state.pool, user_id).await?;
    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        let key: SecurityKey = serde_json::from_value(row.credential.clone())
            .map_err(|err| ApiError::Internal(Box::new(err)))?;
        keys.push((row, key));
    }
    Ok(keys)
}

/// The registration ceremony state stashed on the challenge, carrying the
/// nickname chosen up front so the finish step does not have to re-ask.
#[derive(Serialize, Deserialize)]
struct RegistrationState {
    reg: SecurityKeyRegistration,
    nickname: String,
}

// ---- Settings surface (registration) -----------------------------------

/// The "Security keys" block shown on the two-factor settings page once TOTP is
/// on. The list and remove buttons work without JavaScript; adding a key needs
/// the shim, so its form starts `hidden` and is revealed by the JS — and its
/// status wording rides along as `data-*` attributes, since the shim has no
/// catalog of its own.
pub(crate) fn keys_section(user: &WebUser, creds: &[WebauthnCredential]) -> Markup {
    let locale = user.locale;
    html! {
        fieldset.settings-form__group {
            legend { (locale.text("security-keys-legend")) }
            p.settings-field__hint {
                (locale.text("security-keys-hint"))
            }
            @if creds.is_empty() {
                p.settings-field__hint { (locale.text("security-keys-empty")) }
            } @else {
                ul.two-factor__keys {
                    @for cred in creds {
                        li.two-factor__key {
                            span.two-factor__key-name { (cred.nickname) }
                            form.settings-form--inline method="post"
                                action=(format!("/web/settings/webauthn/{}/delete", cred.id)) {
                                input type="hidden" name="csrf" value=(user.csrf);
                                button.settings-button--danger type="submit" {
                                    (locale.text("security-keys-remove"))
                                }
                            }
                        }
                    }
                }
            }
            div.webauthn-register hidden data-webauthn-register-form
                data-webauthn-name-first=(locale.plain("webauthn-name-first"))
                data-webauthn-prompt=(locale.plain("webauthn-prompt"))
                data-webauthn-dismissed=(locale.plain("webauthn-dismissed"))
                data-webauthn-failed=(locale.plain("webauthn-failed")) {
                label.settings-field {
                    span.settings-field__label { (locale.text("security-keys-name")) }
                    input type="text" data-webauthn-nickname
                        maxlength=(MAX_NICKNAME_LEN)
                        placeholder=(locale.text("security-keys-name-placeholder"));
                }
                button.auth-webauthn__button type="button"
                    data-webauthn-register data-webauthn-csrf=(user.csrf) {
                    (locale.text("security-keys-add"))
                }
                p.settings-field__hint data-webauthn-status role="status" {}
            }
            noscript {
                p.settings-field__hint { (locale.text("security-keys-nojs")) }
            }
        }
    }
}

/// The out-of-band success message with the code left as a placeholder for the
/// shim to substitute, so the whole sentence stays one translatable message.
const OOB_CODE_PLACEHOLDER: &str = "{code}";

fn oob_code_template(locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set("code", OOB_CODE_PLACEHOLDER);
    locale.plain_with("webauthn-oob-code", &args)
}

/// The JS-revealed "Use a security key" affordance on a second-factor prompt.
///
/// Shared by the first-party sign-in and the OAuth authorization prompt: they
/// differ only in which ceremony endpoints the shim posts to, passed as
/// `endpoints` (`None` uses the `/login/webauthn*` default). Stays `hidden`
/// without JavaScript, so the TOTP form remains the fallback.
pub(crate) fn login_prompt(
    challenge_token: &str,
    endpoints: Option<(&str, &str)>,
    locale: Locale,
) -> Markup {
    let (options_url, finish_url) = match endpoints {
        Some((options, finish)) => (Some(options), Some(finish)),
        None => (None, None),
    };
    html! {
        div.auth-webauthn hidden data-webauthn-login
            data-webauthn-options-url=[options_url]
            data-webauthn-finish-url=[finish_url]
            data-webauthn-prompt=(locale.plain("webauthn-prompt"))
            data-webauthn-dismissed=(locale.plain("webauthn-dismissed"))
            data-webauthn-failed=(locale.plain("webauthn-failed"))
            data-webauthn-oob=(oob_code_template(locale)) {
            button.auth-webauthn__button type="button"
                data-webauthn-authenticate data-webauthn-token=(challenge_token) {
                (locale.text("auth-use-security-key"))
            }
            p.auth-webauthn__status data-webauthn-status role="status" {}
        }
    }
}

#[derive(Deserialize)]
pub struct RegisterOptionsRequest {
    csrf: String,
    nickname: String,
}

#[derive(Serialize)]
struct RegisterOptionsResponse {
    challenge_token: String,
    options: CreationChallengeResponse,
}

/// `POST /web/settings/webauthn/options` — begins a registration ceremony,
/// returning the creation options for `navigator.credentials.create` plus the
/// challenge token the finish step echoes back.
pub async fn register_options(
    State(state): State<AppState>,
    user: WebUser,
    Json(req): Json<RegisterOptionsRequest>,
) -> Response {
    if !user.csrf_ok(&req.csrf) {
        return csrf_rejection();
    }
    // The shim renders these rejections verbatim into its status line, so they
    // are the reader's copy, not API wording — every one comes from the catalog.
    let locale = user.locale;
    // WebAuthn keys are second factors layered on TOTP (Mastodon's model): no
    // authenticator app, nothing to add a key to.
    if !user.current.user.otp_required_for_login {
        return ApiError::Unprocessable(locale.text("security-keys-need-totp")).into_response();
    }
    let uid = user.current.user.id;
    let nickname = req.nickname.trim();
    if nickname.is_empty() || nickname.chars().count() > MAX_NICKNAME_LEN {
        return ApiError::Unprocessable(locale.text("security-keys-need-name")).into_response();
    }
    match webauthn_credential::nickname_taken(&state.pool, uid, nickname).await {
        Ok(true) => {
            return ApiError::Unprocessable(locale.text("security-keys-name-taken"))
                .into_response();
        }
        Ok(false) => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    // Exclude the keys already registered so the authenticator refuses to
    // enrol the same device twice.
    let existing = match webauthn_credential::list_by_user(&state.pool, uid).await {
        Ok(rows) => rows,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let exclude: Vec<CredentialID> = existing
        .iter()
        .filter_map(|c| b64url_decode(&c.external_id))
        .collect();

    let handle =
        match webauthn_credential::ensure_webauthn_id(&state.pool, uid, &new_handle().to_string())
            .await
        {
            Ok(handle) => handle,
            Err(err) => return ApiError::from(err).into_response(),
        };
    let Ok(handle) = Uuid::parse_str(&handle) else {
        return ApiError::Internal("stored webauthn handle is not a uuid".into()).into_response();
    };

    // The authenticator's display identity: the e-mail when set, the handle
    // otherwise (purely cosmetic — the credential is keyed by `handle`).
    let identity = user.current.user.email.clone().unwrap_or_else(|| {
        format!(
            "{}@{}",
            user.current.account.username, state.config.account_domain
        )
    });
    let (options, reg_state) = match state.webauthn.start_securitykey_registration(
        handle,
        &identity,
        &identity,
        Some(exclude),
        None,
        None,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!(%err, "starting webauthn registration");
            return ApiError::Internal(Box::new(err)).into_response();
        }
    };

    let raw = generate_secret();
    let challenge_id = match two_factor::create_challenge(
        &state.pool,
        &hash_secret(&raw),
        uid,
        "webauthn_reg",
        None,
    )
    .await
    {
        Ok(id) => id,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let stashed = serde_json::to_value(RegistrationState {
        reg: reg_state,
        nickname: nickname.to_owned(),
    })
    .expect("registration state serializes");
    if let Err(err) = two_factor::set_webauthn_state(&state.pool, challenge_id, &stashed).await {
        return ApiError::from(err).into_response();
    }

    Json(RegisterOptionsResponse {
        challenge_token: raw,
        options,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct RegisterFinishRequest {
    csrf: String,
    challenge_token: String,
    credential: RegisterPublicKeyCredential,
}

/// `POST /web/settings/webauthn` — finishes registration: verifies the
/// attestation against the stashed ceremony state and stores the key.
pub async fn register_finish(
    State(state): State<AppState>,
    user: WebUser,
    Json(req): Json<RegisterFinishRequest>,
) -> Response {
    if !user.csrf_ok(&req.csrf) {
        return csrf_rejection();
    }
    let locale = user.locale;
    let uid = user.current.user.id;
    let challenge = match two_factor::find_challenge(
        &state.pool,
        &hash_secret(&req.challenge_token),
        "webauthn_reg",
    )
    .await
    {
        Ok(Some(challenge)) if challenge.user_id == uid => challenge,
        Ok(_) => {
            return ApiError::Unprocessable(locale.text("security-keys-restart")).into_response();
        }
        Err(err) => return ApiError::from(err).into_response(),
    };
    let Some(stashed) = challenge.webauthn_state.clone() else {
        return ApiError::Unprocessable(locale.text("security-keys-restart")).into_response();
    };
    let Ok(RegistrationState { reg, nickname }) = serde_json::from_value(stashed) else {
        return ApiError::Internal("corrupt webauthn registration state".into()).into_response();
    };

    let key = match state
        .webauthn
        .finish_securitykey_registration(&req.credential, &reg)
    {
        Ok(key) => key,
        Err(err) => {
            tracing::debug!(%err, "webauthn registration finish failed");
            return ApiError::Unprocessable(locale.text("security-keys-register-failed"))
                .into_response();
        }
    };

    let external_id = b64url(key.cred_id());
    let credential = serde_json::to_value(&key).expect("security key serializes");
    match webauthn_credential::create(&state.pool, uid, &external_id, &nickname, &credential, 0)
        .await
    {
        Ok(_) => {}
        Err(DbError::WebauthnNicknameTaken) => {
            return ApiError::Unprocessable(locale.text("security-keys-name-taken"))
                .into_response();
        }
        Err(err) => return ApiError::from(err).into_response(),
    }
    if let Err(err) = two_factor::delete_challenge(&state.pool, challenge.id).await {
        return ApiError::from(err).into_response();
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `POST /web/settings/webauthn/{id}/delete` — removes a key (plain form, no
/// JavaScript needed). Scoped to the session's own credentials.
pub async fn delete_credential(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return super::settings::bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match webauthn_credential::delete(&state.pool, user.current.user.id, id).await {
        Ok(_) => redirect_to(SETTINGS_PATH),
        Err(err) => ApiError::from(err).into_response(),
    }
}

// ---- Login surface ------------------------------------------------------

/// Begins a security-key login assertion for the account behind a live challenge
/// of `context`, stashing the ceremony state on it. Context-agnostic so the web
/// (`"web"`) and OAuth (`"oauth"`) sign-in flows share one implementation — the
/// challenge token is the only secret, so neither flow needs a session.
///
/// There is no signed-in user to take an interface preference from, so the
/// rejections the shim shows are formatted in the request's negotiated locale.
pub(crate) async fn begin_assertion(
    state: &AppState,
    challenge_token: &str,
    context: &str,
    locale: Locale,
) -> Result<RequestChallengeResponse, ApiError> {
    let challenge = two_factor::find_challenge(&state.pool, &hash_secret(challenge_token), context)
        .await?
        .ok_or_else(|| ApiError::Unauthorized(locale.text("webauthn-expired")))?;
    let keys = load_keys(state, challenge.user_id).await?;
    if keys.is_empty() {
        return Err(ApiError::Unprocessable(locale.text("webauthn-no-keys")));
    }
    let creds: Vec<SecurityKey> = keys.into_iter().map(|(_, key)| key).collect();
    let (options, auth_state) = state
        .webauthn
        .start_securitykey_authentication(&creds)
        .map_err(|err| {
            tracing::error!(%err, "starting webauthn authentication");
            ApiError::Internal(Box::new(err))
        })?;
    let stashed = serde_json::to_value(&auth_state).expect("authentication state serializes");
    two_factor::set_webauthn_state(&state.pool, challenge.id, &stashed).await?;
    Ok(options)
}

/// Finishes a security-key login assertion against a live challenge of
/// `context`: verifies the signature, persists the bumped counter, consumes the
/// challenge and returns the sign-in outcome (the authenticated user id, plus
/// the server-held OAuth request the challenge was bound to, if any). Shared by
/// web and OAuth sign-in; the caller decides what a verified
/// user id becomes (a session cookie for the web, an authorization code for
/// OAuth).
pub(crate) async fn verify_assertion(
    state: &AppState,
    challenge_token: &str,
    context: &str,
    credential: &PublicKeyCredential,
    locale: Locale,
) -> Result<super::two_factor::VerifiedChallenge, ApiError> {
    let challenge = two_factor::find_challenge(&state.pool, &hash_secret(challenge_token), context)
        .await?
        .ok_or_else(|| ApiError::Unauthorized(locale.text("webauthn-expired")))?;
    let Some(stashed) = challenge.webauthn_state.clone() else {
        return Err(ApiError::Unprocessable(
            locale.text("webauthn-restart-sign-in"),
        ));
    };
    let auth_state = serde_json::from_value::<SecurityKeyAuthentication>(stashed)
        .map_err(|_| ApiError::Internal("corrupt webauthn authentication state".into()))?;

    let keys = load_keys(state, challenge.user_id).await?;
    let result = match state
        .webauthn
        .finish_securitykey_authentication(credential, &auth_state)
    {
        Ok(result) => result,
        Err(err) => {
            tracing::debug!(%err, "webauthn authentication finish failed");
            // Count it against the challenge's attempt cap, like a wrong TOTP.
            let _ = two_factor::record_challenge_attempt(&state.pool, challenge.id).await;
            return Err(ApiError::Unprocessable(
                locale.text("webauthn-verify-failed"),
            ));
        }
    };

    // The signing key's post-assertion state (bumped signature counter /
    // backup flags — webauthn-rs mutates them inside the serialized
    // credential), computed before the claim so both persist together below.
    let used_id = b64url(result.cred_id());
    let credential_update = keys
        .into_iter()
        .find(|(row, _)| row.external_id == used_id)
        .and_then(|(row, mut key)| {
            (key.update_credential(&result) == Some(true)).then(|| {
                let credential = serde_json::to_value(&key).expect("security key serializes");
                (row.id, credential)
            })
        });

    // Consume the challenge as an atomic single-use claim *before* returning a
    // user id. Two concurrent finishes of one assertion both verify against the
    // same stored ceremony state, so this claim is what guarantees only one of
    // them mints a session/grant; the loser is rejected (finding #35). When the
    // key reported a state change, the claim and the counter update commit as
    // ONE transaction: a completion can never win a session while losing the
    // clone-detection counter, and a failed write rolls the claim back so the
    // user retries against a still-live challenge.
    let claimed = match &credential_update {
        Some((credential_id, credential)) => {
            two_factor::consume_challenge_updating_credential(
                &state.pool,
                challenge.id,
                *credential_id,
                credential,
                i64::from(result.counter()),
            )
            .await?
        }
        None => two_factor::consume_challenge(&state.pool, challenge.id).await?,
    };
    if !claimed {
        return Err(ApiError::Unauthorized(locale.text("webauthn-expired")));
    }

    Ok(super::two_factor::VerifiedChallenge {
        user_id: challenge.user_id,
        oauth_request: challenge.oauth_request.clone(),
    })
}

#[derive(Deserialize)]
pub struct LoginOptionsRequest {
    challenge_token: String,
}

#[derive(Serialize)]
struct LoginOptionsResponse {
    options: RequestChallengeResponse,
}

/// `POST /login/webauthn/options` — begins the login assertion for the account
/// behind a live password challenge. The challenge token itself is the secret;
/// there is no session yet, so no CSRF token.
pub async fn login_options(
    State(state): State<AppState>,
    locale: Locale,
    Json(req): Json<LoginOptionsRequest>,
) -> Response {
    match begin_assertion(&state, &req.challenge_token, "web", locale).await {
        Ok(options) => Json(LoginOptionsResponse { options }).into_response(),
        Err(err) => err.into_response(),
    }
}

#[derive(Deserialize)]
pub struct LoginFinishRequest {
    challenge_token: String,
    credential: PublicKeyCredential,
}

/// `POST /login/webauthn` — finishes the login assertion, persists the updated
/// signature counter, consumes the challenge and installs the session cookie on
/// a JSON response the shim navigates from.
pub async fn login_finish(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    locale: Locale,
    headers: HeaderMap,
    Json(req): Json<LoginFinishRequest>,
) -> Response {
    let app = match ensure_web_app(&state).await {
        Ok(app) => app,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = crate::instance_policy::ensure_ip_login_allowed(&state.pool, remote_ip).await
    {
        return err.into_response();
    }
    match verify_assertion(&state, &req.challenge_token, "web", &req.credential, locale).await {
        Ok(verified) => {
            complete_login_json(
                &state,
                &app,
                verified.user_id,
                "webauthn",
                &headers,
                remote_ip,
            )
            .await
        }
        Err(err) => err.into_response(),
    }
}
