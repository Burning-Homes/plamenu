//! Two-factor authentication.
//!
//! This module owns two things: the shared TOTP/recovery-code verification
//! used by both challenge flows (web login and OAuth authorize), and the
//! settings surface where a user enrols an authenticator, views their
//! recovery codes and turns 2FA off again.
//!
//! Pending-login state lives in a `two_factor_challenges` row keyed by a
//! random token; the raw token round-trips through a hidden form field while
//! only its SHA-256 hash is stored, so a database dump cannot resume a login.

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, PreEscaped, html};
use plamenu_db::{two_factor, user};
use qrcode::QrCode;
use qrcode::render::svg;

use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
    settings_shell,
};
use crate::auth::{self, PasswordPolicy, hash_secret};
use crate::error::ApiError;
use crate::instance_policy::RemoteIp;
use crate::state::AppState;

/// How many hex characters each recovery code holds (8 random bytes).
const RECOVERY_CODE_HEX_BYTES: usize = 8;

// ---- Shared verification ----------------------------------------------

/// Why a submitted second factor did not sign the user in.
pub(crate) enum ChallengeError {
    /// The code was wrong but the challenge is still live — re-prompt.
    WrongCode,
    /// The challenge is gone (expired, attempt-capped, or never existed) —
    /// the user must start the login over.
    Invalid,
    /// A database error while checking.
    Db(ApiError),
}

/// Seconds since the Unix epoch, for TOTP's time window.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A successfully completed second factor: who signed in, and — for the OAuth
/// context — the server-held authorization request the challenge was bound to
/// at creation. OAuth callers mint the grant from that stored request, never
/// from resubmitted form values, so a captured challenge token cannot finish a
/// different authorization request than the one the password proved.
pub(crate) struct VerifiedChallenge {
    pub user_id: i64,
    pub oauth_request: Option<two_factor::OauthChallengeRequest>,
}

/// Verifies a submitted TOTP *or* recovery code against a live challenge and,
/// on success, consumes both the code and the challenge and returns the
/// sign-in outcome.
///
/// Shared by the web-login (`context = "web"`) and OAuth-authorize
/// (`context = "oauth"`) flows so the second-factor policy lives in one place.
pub(crate) async fn verify_challenge(
    state: &AppState,
    raw_token: &str,
    context: &str,
    code: &str,
) -> Result<VerifiedChallenge, ChallengeError> {
    let challenge = two_factor::find_challenge(&state.pool, &hash_secret(raw_token), context)
        .await
        .map_err(|e| ChallengeError::Db(e.into()))?
        .ok_or(ChallengeError::Invalid)?;
    let user = user::find_by_id(&state.pool, challenge.user_id)
        .await
        .map_err(|e| ChallengeError::Db(e.into()))?
        .ok_or(ChallengeError::Invalid)?;
    let verified = VerifiedChallenge {
        user_id: user.id,
        oauth_request: challenge.oauth_request.clone(),
    };

    // TOTP first: decrypt the secret, verify within the drift window, then
    // atomically claim the timestep so the same code cannot be replayed.
    if let Some(secret_box) = crate::crypto::otp_box(&state.config)
        && let Some(encrypted) = user.otp_secret.as_deref()
        && let Some(secret_bytes) = secret_box.decrypt(encrypted)
    {
        let secret = String::from_utf8_lossy(&secret_bytes);
        if let Some(step) = crate::totp::verify(&secret, code, now_epoch()) {
            let claimed = user::consume_otp_timestep(&state.pool, user.id, step)
                .await
                .map_err(|e| ChallengeError::Db(e.into()))?;
            if claimed {
                two_factor::delete_challenge(&state.pool, challenge.id)
                    .await
                    .map_err(|e| ChallengeError::Db(e.into()))?;
                return Ok(verified);
            }
            // A replayed code inside its drift window: count it as wrong.
            return wrong_or_invalid(state, challenge.id).await;
        }
    }

    // Fall back to a one-time recovery code (normalised the same way it was
    // shown: whitespace stripped, lower-case).
    let recovery: String = code.split_whitespace().collect::<String>().to_lowercase();
    if !recovery.is_empty()
        && two_factor::consume_backup_code(&state.pool, user.id, &hash_secret(&recovery))
            .await
            .map_err(|e| ChallengeError::Db(e.into()))?
    {
        two_factor::delete_challenge(&state.pool, challenge.id)
            .await
            .map_err(|e| ChallengeError::Db(e.into()))?;
        return Ok(verified);
    }

    wrong_or_invalid(state, challenge.id).await
}

/// Counts a failed attempt: a still-live challenge is [`ChallengeError::WrongCode`],
/// a now-capped (and deleted) one is [`ChallengeError::Invalid`].
async fn wrong_or_invalid(
    state: &AppState,
    challenge_id: i64,
) -> Result<VerifiedChallenge, ChallengeError> {
    match two_factor::record_challenge_attempt(&state.pool, challenge_id).await {
        Ok(true) => Err(ChallengeError::WrongCode),
        Ok(false) => Err(ChallengeError::Invalid),
        Err(e) => Err(ChallengeError::Db(e.into())),
    }
}

/// Ten fresh recovery codes (16 lower-case hex chars each).
fn generate_recovery_codes() -> Vec<String> {
    (0..two_factor::BACKUP_CODE_COUNT)
        .map(|_| {
            let mut bytes = [0u8; RECOVERY_CODE_HEX_BYTES];
            getrandom::fill(&mut bytes).expect("system randomness available");
            let mut code = String::with_capacity(RECOVERY_CODE_HEX_BYTES * 2);
            for byte in bytes {
                write!(code, "{byte:02x}").expect("writing to a String never fails");
            }
            code
        })
        .collect()
}

// ---- Settings surface --------------------------------------------------

const PATH: &str = "/settings/security";

/// The flash for an `?error=` code. The two length messages quote the shared
/// password policy's limits rather than repeating them, so the catalog copy
/// cannot drift from [`auth::validate_password`].
fn tf_error(code: Option<&str>, locale: Locale) -> Option<String> {
    let limited = |id: &str, limit: usize| {
        let mut args = FluentArgs::new();
        args.set("limit", i64::try_from(limit).unwrap_or(i64::MAX));
        locale.text_with(id, &args)
    };
    match code {
        Some("current_password") => Some(locale.text("security-error-current-password")),
        Some("password_mismatch") => Some(locale.text("security-error-password-mismatch")),
        Some("password_empty") => Some(locale.text("security-error-password-empty")),
        Some("password_short") => {
            Some(limited("security-error-password-short", auth::PASSWORD_MIN))
        }
        Some("password_long") => Some(limited("security-error-password-long", auth::PASSWORD_MAX)),
        Some("code") => Some(locale.text("security-error-code")),
        Some("no_secret") => Some(locale.text("security-error-no-secret")),
        _ => None,
    }
}

/// The catalog rendering of a shared password-policy rejection. Lives beside
/// [`tf_error`] so the three length messages have one home; the anonymous reset
/// form (`web::password`) shows the same copy for the same policy.
pub(crate) fn password_policy_message(policy: PasswordPolicy, locale: Locale) -> String {
    let code = match policy {
        PasswordPolicy::Empty => "password_empty",
        PasswordPolicy::TooShort => "password_short",
        PasswordPolicy::TooLong => "password_long",
    };
    tf_error(Some(code), locale).unwrap_or_else(|| policy.message())
}

fn tf_saved(code: Option<&str>, locale: Locale) -> Option<String> {
    let id = match code {
        Some("password") => "security-saved-password",
        Some("disabled") => "security-saved-disabled",
        Some("session_revoked") => "security-saved-session-revoked",
        Some("app_revoked") => "security-saved-app-revoked",
        Some("alert") => "security-saved-alert",
        _ => return None,
    };
    Some(locale.text(id))
}

/// `GET /settings/security` — the change-password form, then whichever
/// two-factor enrolment state applies.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Markup, ApiError> {
    let u = &user.current.user;
    let locale = user.locale;
    let two_factor_section = if crate::crypto::otp_box(&state.config).is_none() {
        not_configured_notice(locale)
    } else if u.otp_required_for_login {
        let remaining = two_factor::backup_codes_remaining(&state.pool, u.id).await?;
        let keys = plamenu_db::webauthn_credential::list_by_user(&state.pool, u.id).await?;
        enabled_status(&user, remaining, &keys)
    } else if let Some(secret) = provisional_secret(&state, u.otp_secret.as_deref()) {
        provisional_form(&state, &user, &secret)
    } else {
        enable_form(&user)
    };
    let saved = tf_saved(query.saved.as_deref(), locale);
    let account_access = super::sessions::account_access(&state, &user).await?;
    // The new-IP alert only does anything with a mail relay configured, so the
    // toggle is shown only then.
    let sign_in_alerts = if crate::mailer::enabled(&state) {
        let enabled = plamenu_db::user::new_ip_sign_in_alert(&state.pool, u.id).await?;
        Some(sign_in_alert_form(&user, enabled))
    } else {
        None
    };
    let error = tf_error(query.error.as_deref(), locale);
    let body = html! {
        (saved_flash(saved.is_some(), saved.as_deref().unwrap_or_default()))
        (error_flash(error.as_deref()))
        // The security page stacks several independent forms and the account-
        // access cards; the stack gives them one consistent rhythm (the plain
        // settings body has no gap, so without it the blocks and their buttons
        // run together).
        div.settings-stack {
            (password_form(&user))
            section.settings-stack__item {
                h3.settings-subhead { (locale.text("security-two-factor-title")) }
                (two_factor_section)
            }
            @if let Some(form) = sign_in_alerts {
                section.settings-stack__item {
                    h3.settings-subhead { (locale.text("security-sign-in-alerts-title")) }
                    (form)
                }
            }
            (account_access)
        }
    };
    Ok(settings_shell(
        &user,
        PATH,
        &locale.text("settings-section-security"),
        &body,
    ))
}

/// The opt-in for a notification e-mail when the account is signed in to from an
/// IP it has never used before. Off by default.
fn sign_in_alert_form(user: &WebUser, enabled: bool) -> Markup {
    let locale = user.locale;
    html! {
        form.settings-form method="post" action="/web/settings/security/sign-in-alert" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                label.settings-toggle {
                    input type="checkbox" name="new_ip_sign_in_alert" value="1" checked[enabled];
                    span.settings-toggle__label { (locale.text("security-alert-label")) }
                }
                p.settings-field__hint { (locale.text("security-alert-hint")) }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("common-save")) }
            }
        }
    }
}

/// `POST /web/settings/security/sign-in-alert` — store the new-IP alert opt-in.
pub async fn sign_in_alert_action(
    State(state): State<AppState>,
    user: WebUser,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let enabled = field(&pairs, "new_ip_sign_in_alert").is_some();
    match plamenu_db::user::set_new_ip_sign_in_alert(&state.pool, user.current.user.id, enabled)
        .await
    {
        Ok(_) => redirect_to("/settings/security?saved=alert"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The change-password form. Lives here rather than on the Account page:
/// sign-in credentials and second factors belong on one Security surface.
fn password_form(user: &WebUser) -> Markup {
    let locale = user.locale;
    html! {
        form.settings-form method="post" action="/web/settings/security/password" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("security-password-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-current-password")) }
                    input type="password" name="current_password"
                        autocomplete="current-password" required;
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-new-password")) }
                    input type="password" name="new_password"
                        autocomplete="new-password" minlength=(auth::PASSWORD_MIN)
                        maxlength=(auth::PASSWORD_MAX) required;
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-confirm-password")) }
                    input type="password" name="confirm_password"
                        autocomplete="new-password" minlength=(auth::PASSWORD_MIN)
                        maxlength=(auth::PASSWORD_MAX) required;
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("security-change-password")) }
            }
        }
    }
}

/// Decrypts the pending secret, if one is stored and readable.
fn provisional_secret(state: &AppState, encrypted: Option<&str>) -> Option<String> {
    let secret_box = crate::crypto::otp_box(&state.config)?;
    let bytes = secret_box.decrypt(encrypted?)?;
    String::from_utf8(bytes).ok()
}

fn not_configured_notice(locale: Locale) -> Markup {
    html! {
        section.settings-form__group {
            p.settings-field__hint {
                (locale.markup("security-not-configured", &[(
                    "setting",
                    html! { code { "encryption_secret" } },
                )]))
            }
        }
    }
}

fn enable_form(user: &WebUser) -> Markup {
    let locale = user.locale;
    html! {
        p.settings-field__hint { (locale.text("security-two-factor-intro")) }
        form.settings-form method="post" action="/web/settings/two_factor/setup" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("security-enable-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-current-password")) }
                    input type="password" name="current_password"
                        autocomplete="current-password" required;
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("security-set-up")) }
            }
        }
    }
}

fn provisional_form(state: &AppState, user: &WebUser, secret: &str) -> Markup {
    // The authenticator-app label: the e-mail when set, the handle otherwise.
    let label = user.current.user.email.clone().unwrap_or_else(|| {
        format!(
            "{}@{}",
            user.current.account.username, state.config.account_domain
        )
    });
    let uri = crate::totp::provisioning_uri(secret, &state.config.account_domain, &label);
    let locale = user.locale;
    html! {
        fieldset.settings-form__group {
            legend { (locale.text("security-scan-legend")) }
            p.settings-field__hint { (locale.text("security-scan-hint")) }
            div.two-factor__qr { (qr_svg(&uri)) }
            p.settings-field__hint {
                (locale.markup("security-setup-key", &[(
                    "key",
                    html! { code { (secret) } },
                )]))
            }
        }
        form.settings-form method="post" action="/web/settings/two_factor/confirm" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("security-confirm-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-auth-code")) }
                    input type="text" name="code" inputmode="numeric"
                        autocomplete="one-time-code" autofocus required;
                    span.settings-field__hint { (locale.text("security-auth-code-hint")) }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("security-enable")) }
            }
        }
    }
}

fn enabled_status(
    user: &WebUser,
    remaining: i64,
    keys: &[plamenu_db::webauthn_credential::WebauthnCredential],
) -> Markup {
    let locale = user.locale;
    let mut args = FluentArgs::new();
    args.set("count", remaining);
    html! {
        section.settings-form__group {
            p.settings__saved role="status" { (locale.text("security-two-factor-on")) }
            p.settings-field__hint {
                (locale.text_with("security-recovery-remaining", &args))
            }
        }
        (super::webauthn::keys_section(user, keys))
        form.settings-form method="post" action="/web/settings/two_factor/recovery_codes" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("security-recovery-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-current-password")) }
                    input type="password" name="current_password"
                        autocomplete="current-password" required;
                }
                p.settings-field__hint { (locale.text("security-recovery-regenerate-hint")) }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("security-recovery-regenerate")) }
            }
        }
        form.settings-form method="post" action="/web/settings/two_factor/disable" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group.settings-form__group--danger {
                legend { (locale.text("security-turn-off-legend")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("security-current-password")) }
                    input type="password" name="current_password"
                        autocomplete="current-password" required;
                }
            }
            div.settings-form__actions {
                button.settings-button--danger type="submit" {
                    (locale.text("security-disable"))
                }
            }
        }
    }
}

/// The one-time page that shows freshly minted recovery codes. They are never
/// retrievable again, so the copy stresses saving them now.
fn recovery_codes_page(user: &WebUser, codes: &[String]) -> Markup {
    let locale = user.locale;
    let body = html! {
        section.settings-form__group {
            p.settings__saved role="status" { (locale.text("security-recovery-save-now")) }
            p.settings-field__hint { (locale.text("security-recovery-once-hint")) }
            ul.two-factor__codes {
                @for code in codes {
                    li { code { (code) } }
                }
            }
            p { a href=(PATH) { (locale.text("security-recovery-saved-link")) } }
        }
    };
    settings_shell(user, PATH, &locale.text("security-recovery-title"), &body)
}

/// Inline SVG for the enrolment QR code (no JavaScript needed). The renderer
/// emits an XML prolog that has no place inside an HTML document, so it is
/// stripped down to the `<svg>` element.
fn qr_svg(uri: &str) -> Markup {
    let rendered = QrCode::new(uri.as_bytes())
        .map(|code| {
            code.render::<svg::Color>()
                .min_dimensions(200, 200)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default();
    let svg_only = rendered
        .find("<svg")
        .map_or_else(String::new, |i| rendered[i..].to_owned());
    PreEscaped(svg_only)
}

// ---- Actions -----------------------------------------------------------

/// Validates CSRF, then the current password, returning the redirect to send
/// back on failure (or `None` to proceed) — the `update_password_action`
/// pattern, shared by every password-gated two-factor action.
async fn password_gate(user: &WebUser, pairs: &[(String, String)]) -> Option<Response> {
    if !user.csrf_ok(field(pairs, "csrf").unwrap_or_default()) {
        return Some(csrf_rejection());
    }
    if !auth::verify_password_gated(
        field(pairs, "current_password")
            .unwrap_or_default()
            .to_owned(),
        user.current.user.password_hash.clone(),
    )
    .await
    {
        return Some(redirect_to(&format!("{PATH}?error=current_password")));
    }
    None
}

/// `POST /web/settings/security/password` — change the sign-in password,
/// gated on the current one. A successful change signs the account out of every
/// other session and app token and rotates this browser onto a
/// fresh token.
pub async fn password_action(
    State(state): State<AppState>,
    user: WebUser,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if let Some(response) = password_gate(&user, &pairs).await {
        return response;
    }
    let new_password = field(&pairs, "new_password").unwrap_or_default();
    // The length policy is shared with registration, reset, and the CLI (#36).
    if let Err(policy) = auth::validate_password(new_password) {
        let code = match policy {
            PasswordPolicy::Empty => "password_empty",
            PasswordPolicy::TooShort => "password_short",
            PasswordPolicy::TooLong => "password_long",
        };
        return redirect_to(&format!("{PATH}?error={code}"));
    }
    if Some(new_password) != field(&pairs, "confirm_password") {
        return redirect_to(&format!("{PATH}?error=password_mismatch"));
    }
    let hash = match auth::hash_password_gated(new_password.to_owned()).await {
        Ok(hash) => hash,
        Err(err) => return err.into_response(),
    };
    let user_id = user.current.user.id;
    // The password change and the sign-out-everywhere revocation commit as one
    // transaction: every session and app token — including one
    // an intruder copied — is revoked atomically with the new password, so there
    // is no window where the password has changed but a stolen token survives a
    // failed revocation. This is the exact action the sign-in-alert copy tells a
    // user to take on unfamiliar activity, so it must never half-apply.
    match user::change_password_and_revoke(&state.pool, user_id, &hash).await {
        Ok(Some(_)) => {}
        Ok(None) => return ApiError::NotFound.into_response(),
        Err(err) => return ApiError::from(err).into_response(),
    }
    // Rotate this browser onto a fresh token so the device the change was made
    // from stays signed in while the revoked (possibly copied) token is dead.
    match super::session::reissue_session(&state, &headers, remote_ip, user_id).await {
        Ok((active, roster)) => (
            StatusCode::SEE_OTHER,
            AppendHeaders([
                (header::LOCATION, format!("{PATH}?saved=password")),
                (header::SET_COOKIE, active),
                (header::SET_COOKIE, roster),
            ]),
        )
            .into_response(),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/settings/two_factor/setup` — stash a provisional secret so the
/// page can show its QR code. Password-gated.
pub async fn setup_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if let Some(rejection) = password_gate(&user, &pairs).await {
        return rejection;
    }
    if crate::crypto::otp_box(&state.config).is_none() {
        return redirect_to(PATH);
    }
    let secret_box = crate::crypto::otp_box(&state.config).expect("checked above");
    let encrypted = secret_box.encrypt(crate::totp::generate_secret().as_bytes());
    match user::set_otp_secret(&state.pool, user.current.user.id, &encrypted).await {
        Ok(()) => redirect_to(PATH),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/two_factor/confirm` — prove the authenticator works,
/// then enable 2FA and hand back the recovery codes. Gated by the code itself.
pub async fn confirm_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let Some(secret) = provisional_secret(&state, user.current.user.otp_secret.as_deref()) else {
        return redirect_to(&format!("{PATH}?error=no_secret"));
    };
    let code = field(&pairs, "code").unwrap_or_default();
    let Some(step) = crate::totp::verify(&secret, code, now_epoch()) else {
        return redirect_to(&format!("{PATH}?error=code"));
    };
    // Claim the timestep, enable the login requirement, and replace the recovery
    // codes as one transaction: a failure at any step rolls the
    // whole enrollment back, so 2FA is never left enabled with no freshly shown
    // recovery codes. The plaintext codes are shown only once this commits.
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_secret(c)).collect();
    match two_factor::confirm_otp_enrollment(&state.pool, user.current.user.id, step, &hashes).await
    {
        Ok(two_factor::OtpEnrollment::Enabled) => {
            recovery_codes_page(&user, &codes).into_response()
        }
        Ok(two_factor::OtpEnrollment::CodeReused) => redirect_to(&format!("{PATH}?error=code")),
        Ok(two_factor::OtpEnrollment::NoSecret) => redirect_to(&format!("{PATH}?error=no_secret")),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/two_factor/recovery_codes` — regenerate and show once.
/// Password-gated.
pub async fn recovery_codes_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if let Some(rejection) = password_gate(&user, &pairs).await {
        return rejection;
    }
    if !user.current.user.otp_required_for_login {
        return redirect_to(PATH);
    }
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_secret(c)).collect();
    match two_factor::replace_backup_codes(&state.pool, user.current.user.id, &hashes).await {
        Ok(()) => recovery_codes_page(&user, &codes).into_response(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/two_factor/disable` — clear the secret, requirement and
/// recovery codes. Password-gated.
pub async fn disable_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if let Some(rejection) = password_gate(&user, &pairs).await {
        return rejection;
    }
    match user::disable_otp(&state.pool, user.current.user.id).await {
        Ok(()) => redirect_to(&format!("{PATH}?saved=disabled")),
        Err(err) => ApiError::from(err).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fluent brackets interpolated values in directional isolates; they are
    /// invisible to a reader but not to `assert_eq!`.
    fn strip_isolates(text: &str) -> String {
        text.replace(['\u{2068}', '\u{2069}'], "")
    }

    #[test]
    fn password_length_flashes_match_the_shared_policy() {
        // The `?error=` flashes on the signed-in change form come from the
        // catalog, but the policy that produces them lives in `auth` (#36).
        // The messages interpolate that policy's limits, so an English
        // rendering must read exactly as the shared policy does — a change to
        // PASSWORD_MIN/MAX can never leave the form quoting a stale number.
        let english = Locale::default();
        assert_eq!(
            tf_error(Some("password_short"), english).map(|m| strip_isolates(&m)),
            Some(PasswordPolicy::TooShort.message())
        );
        assert_eq!(
            tf_error(Some("password_long"), english).map(|m| strip_isolates(&m)),
            Some(PasswordPolicy::TooLong.message())
        );
        assert_eq!(
            tf_error(Some("password_empty"), english),
            Some(PasswordPolicy::Empty.message())
        );
    }
}
