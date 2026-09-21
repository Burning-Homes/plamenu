//! Self-service sign-up pages — the web counterpart of Mastodon's
//! Devise `registrations`/`confirmations` views: `/signup`, and the
//! `/auth/confirmation` landing the e-mailed link points at. Sign-ups made
//! here go through the same [`crate::registration`] service as
//! `POST /api/v1/accounts`, attributed to the built-in web app.

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::instance_settings::{self, RegistrationsMode};
use serde::Deserialize;

use super::i18n::Locale;
use super::layout;
use super::session::{MaybeWebUser, ensure_web_app};
use crate::error::ApiError;
use crate::instance_policy::RemoteIp;
use crate::registration::{self, SignUpParams};
use crate::state::AppState;

/// The pre-filled values a failed submission round-trips back into the form.
#[derive(Default)]
struct FormValues {
    username: String,
    email: String,
    reason: String,
    /// Carried through a hidden field so invite-based sign-ups survive a
    /// validation re-render.
    invite_code: String,
    /// The chosen IANA zone, so a failed submission does not silently reset it
    /// to the server default. Empty means "server default".
    time_zone: String,
}

#[derive(Deserialize)]
pub struct SignupQuery {
    invite_code: Option<String>,
}

/// `GET /signup` — the registration form (or the closed-registrations
/// notice). Signed-in visitors get the form too — the created account joins
/// the switcher roster next to the current one. A valid `invite_code`
/// query opens the form even while registrations are closed.
pub async fn signup_form(
    State(state): State<AppState>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
    Query(query): Query<SignupQuery>,
) -> Result<Response, Response> {
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let settings = instance_settings::get(&state.pool).await.map_err(api_err)?;
    let invite_code = match query.invite_code.as_deref().map(str::trim) {
        Some(code) if !code.is_empty() => plamenu_db::invite::find_valid_by_code(&state.pool, code)
            .await
            .map_err(api_err)?
            .map(|invite| invite.code),
        _ => None,
    };
    if invite_code.is_none()
        && !registration::open_for_registrations(&state)
            .await
            .map_err(IntoResponse::into_response)?
    {
        return Ok(closed_page(&state, locale).await.into_response());
    }
    let values = FormValues {
        invite_code: invite_code.unwrap_or_default(),
        ..FormValues::default()
    };
    Ok(signup_page(
        &state,
        settings.registrations_mode(),
        settings.min_age,
        &values,
        &[],
        session.is_some(),
        locale,
    )
    .await
    .into_response())
}

async fn closed_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("signup-closed-title")) }
            p { (locale.text("signup-closed-body")) }
            p { a href="/login" { (locale.text("nav-sign-in")) } }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("signup-closed-page-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

/// The sign-up form's zone picker (G4). Without it every account starts on
/// UTC and most people never find the preference; the same inventory backs the
/// settings dropdown, so a zone offered here is offered there.
fn time_zone_field(current: &str, locale: Locale) -> Markup {
    html! {
        label {
            (locale.text("signup-time-zone"))
            select name="time_zone" {
                option value="" selected[current.is_empty()] {
                    (locale.text("signup-time-zone-default"))
                }
                @for (name, label) in crate::web::clock::zone_options() {
                    option value=(name) selected[current == name] { (label) }
                }
            }
            span.settings-field__hint { (locale.text("signup-time-zone-help")) }
        }
    }
}

async fn signup_page(
    state: &AppState,
    mode: RegistrationsMode,
    min_age: i32,
    values: &FormValues,
    errors: &[String],
    signed_in: bool,
    locale: Locale,
) -> Markup {
    let mut minimum_age_args = fluent_bundle::FluentArgs::new();
    minimum_age_args.set("years", min_age);
    let minimum_age_help = locale.text_with("signup-minimum-age", &minimum_age_args);
    let content = html! {
        section.auth-card {
            h1 { (locale.text("signup-title")) }
            @if signed_in {
                p.settings-field__hint {
                    (locale.text("signup-already-signed-in"))
                }
            }
            @if mode == RegistrationsMode::Approved {
                p { (locale.text("signup-reviewed")) }
            }
            @if !errors.is_empty() {
                ul.form-error role="alert" {
                    @for error in errors { li { (error) } }
                }
            }
            form.auth-form method="post" action="/signup" {
                @if !values.invite_code.is_empty() {
                    input type="hidden" name="invite_code" value=(values.invite_code);
                }
                label {
                    (locale.text("signup-username"))
                    input type="text" name="username" value=(values.username)
                        autocomplete="username" required autofocus;
                    span.settings-field__hint { (locale.text("signup-username-help")) }
                }
                label {
                    (locale.text("signup-email"))
                    input type="email" name="email" value=(values.email)
                        autocomplete="email";
                    span.settings-field__hint {
                        @if crate::mailer::enabled(state) {
                            (locale.text("signup-email-help-enabled"))
                        } @else {
                            (locale.text("signup-email-help-disabled"))
                        }
                    }
                }
                label {
                    (locale.text("auth-password"))
                    input type="password" name="password"
                        autocomplete="new-password" minlength="8" required;
                }
                label {
                    (locale.text("signup-confirm-password"))
                    input type="password" name="password_confirmation"
                        autocomplete="new-password" minlength="8" required;
                }
                @if min_age > 0 {
                    label {
                        (locale.text("signup-date-of-birth"))
                        input type="date" name="date_of_birth" required;
                        span.settings-field__hint {
                            (minimum_age_help)
                        }
                    }
                }
                (time_zone_field(&values.time_zone, locale))
                @if mode == RegistrationsMode::Approved {
                    label {
                        (locale.text("signup-reason"))
                        textarea name="reason" rows="3" maxlength="420" {
                            (values.reason)
                        }
                    }
                }
                label.form-check {
                    input type="checkbox" name="agreement" value="1" required;
                    span {
                        (locale.text("signup-agreement-prefix")) " "
                        a href="/rules" target="_blank" { (locale.text("signup-server-rules")) }
                    }
                }
                altcha-widget
                    challenge="/signup/altcha/challenge"
                    name="altcha"
                    auto="onsubmit"
                    language=(locale.tag().split('-').next().unwrap_or("en")) {}
                button type="submit" { (locale.text("auth-sign-up")) }
            }
            script type="module"
                src={ "/assets/altcha.min.js?v=" (super::assets::ALTCHA_VERSION) } {}
            p.auth-card__alt {
                (locale.text("signup-have-account")) " "
                a href="/login" { (locale.text("nav-sign-in")) }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("auth-sign-up"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

#[derive(Deserialize)]
pub struct SignupForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password_confirmation: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    invite_code: String,
    agreement: Option<String>,
    /// Captured client-side from the browser's locale; an unknown or
    /// absent value is simply dropped.
    #[serde(default)]
    time_zone: String,
    /// Date of birth, only required/validated when the server sets a
    /// minimum age.
    ///
    /// **Deliberately not zone-aware.** A birth date is a calendar date, not
    /// an instant; interpreting it in a zone would move it a day for anyone
    /// east or west of UTC and could flip an age check. It stays the plain ISO
    /// date the `<input type="date">` submits — see the G9 exemption in
    /// `TIMEZONE_HANDOFF.md`.
    #[serde(default)]
    date_of_birth: String,
    /// Base64 JSON proof populated by the self-hosted ALTCHA widget.
    #[serde(default)]
    altcha: String,
}

/// `GET /signup/altcha/challenge` — a fresh, signed, non-cacheable proof-of-work
/// challenge for the widget. It is intentionally same-origin and needs no CORS.
pub async fn altcha_challenge(State(state): State<AppState>) -> Result<Response, Response> {
    let challenge = state.altcha.challenge().map_err(|error| {
        tracing::error!(%error, "failed to issue ALTCHA sign-up challenge");
        ApiError::Internal("could not issue anti-spam challenge".into()).into_response()
    })?;
    Ok((
        [
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Json(challenge),
    )
        .into_response())
}

/// `POST /signup`.
///
/// A successful open registration signs the browser straight in (the
/// `complete_login` arm below), which makes this a pre-session credential POST
/// exactly like `/login` and the OAuth consent submit: without a same-origin
/// ceremony an attacker's page could cross-site submit credentials of its own
/// choosing and leave the victim's browser holding a session for an
/// attacker-controlled account — login CSRF / session swapping,
/// which covered the other credential POSTs but not this one).
#[allow(
    clippy::too_many_lines,
    reason = "the handler preserves form state across origin, proof, and account validation"
)]
pub async fn signup_submit(
    State(state): State<AppState>,
    RemoteIp(remote_ip): RemoteIp,
    MaybeWebUser(session): MaybeWebUser,
    headers: HeaderMap,
    Form(form): Form<SignupForm>,
) -> Result<Response, Response> {
    if !crate::auth::same_origin_request(&headers, &state.config.domain) {
        return Err(super::session::cross_origin_login_rejection());
    }
    let signed_in = session.is_some();
    let interface_locale = session.as_ref().map_or_else(
        || {
            Locale::negotiate(
                None,
                headers
                    .get(axum::http::header::ACCEPT_LANGUAGE)
                    .and_then(|value| value.to_str().ok()),
            )
        },
        |user| user.locale,
    );
    let settings = instance_settings::get(&state.pool).await.map_err(api_err)?;
    let values = FormValues {
        username: form.username.trim().to_owned(),
        email: form.email.trim().to_owned(),
        reason: form.reason.clone(),
        invite_code: form.invite_code.trim().to_owned(),
        // Normalised so a re-render never re-selects a value the inventory
        // doesn't offer; `NewUser` stores the same normalised form.
        time_zone: crate::time_zones::normalize(&form.time_zone)
            .unwrap_or_default()
            .to_owned(),
    };
    let mode = settings.registrations_mode();

    // Preserve the established closed-registration response for ordinary
    // unsolicited POSTs. Open registrations and invite attempts must prove
    // browser work before any credential hashing or account validation.
    if (mode != RegistrationsMode::None || !values.invite_code.is_empty())
        && state.altcha.verify_and_consume(&form.altcha).is_err()
    {
        let page = signup_page(
            &state,
            mode,
            settings.min_age,
            &values,
            &[interface_locale.text("signup-error-altcha")],
            signed_in,
            interface_locale,
        )
        .await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }

    if form.password != form.password_confirmation {
        let page = signup_page(
            &state,
            mode,
            settings.min_age,
            &values,
            &[interface_locale.text("security-error-password-mismatch")],
            signed_in,
            interface_locale,
        )
        .await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }

    let app = ensure_web_app(&state)
        .await
        .map_err(IntoResponse::into_response)?;
    let signup_locale = crate::auth::accept_language_primary(&headers);
    let result = registration::sign_up(
        &state,
        app.id,
        remote_ip,
        SignUpParams {
            username: &values.username,
            email: (!values.email.is_empty()).then_some(values.email.as_str()),
            password: &form.password,
            agreement: form.agreement.is_some(),
            locale: signup_locale.as_deref(),
            reason: (mode == RegistrationsMode::Approved && !form.reason.trim().is_empty())
                .then_some(form.reason.trim()),
            invite_code: (!values.invite_code.is_empty()).then_some(values.invite_code.as_str()),
            time_zone: crate::time_zones::normalize(&form.time_zone),
            date_of_birth: Some(form.date_of_birth.trim()).filter(|d| !d.is_empty()),
        },
    )
    .await;

    match result {
        // Which page fits depends on what the sign-up still has pending: a
        // mailed confirmation, the approval queue, or nothing at all — a
        // ready account is signed in on the spot (joining the switcher
        // roster next to any accounts already held).
        Ok(user) if !user.confirmed() => {
            Ok(check_inbox_page(&state, &values.email, interface_locale)
                .await
                .into_response())
        }
        Ok(user) if !user.approved => Ok(pending_approval_page(&state, interface_locale)
            .await
            .into_response()),
        Ok(user) => Ok(super::session::complete_login(
            &state, &app, user.id, "signup", &headers, remote_ip,
        )
        .await),
        Err(ApiError::Validation { details, .. }) => {
            let errors = validation_messages(&details, interface_locale);
            let page = signup_page(
                &state,
                mode,
                settings.min_age,
                &values,
                &errors,
                signed_in,
                interface_locale,
            )
            .await;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(ApiError::Forbidden(_)) => {
            Ok(closed_page(&state, interface_locale).await.into_response())
        }
        Err(other) => Err(other.into_response()),
    }
}

/// The form banner's sentences, out of the API's `details` object.
///
/// The API keeps Mastodon's `{attribute, error, description}` shape verbatim
/// (clients parse it); the web form re-states the same failures from the
/// catalog, keyed by attribute and error code. An unrecognized pair falls back
/// to the English description rather than dropping the failure silently.
fn validation_messages(details: &serde_json::Value, locale: Locale) -> Vec<String> {
    let Some(map) = details.as_object() else {
        return vec![locale.text("signup-error-generic")];
    };
    let mut messages = Vec::new();
    for (attribute, entries) in map {
        for entry in entries.as_array().into_iter().flatten() {
            let code = entry["error"].as_str().unwrap_or_default();
            if let Some(message) = validation_message(attribute, code, locale) {
                messages.push(message);
            } else if let Some(description) = entry["description"].as_str() {
                messages.push(format!("{attribute} {description}."));
            }
        }
    }
    if messages.is_empty() {
        messages.push(locale.text("signup-error-generic"));
    }
    messages
}

/// One `(attribute, code)` pair as catalog copy. The length messages quote the
/// limit that produced them, so the copy cannot drift from the validator.
fn validation_message(attribute: &str, code: &str, locale: Locale) -> Option<String> {
    let limited = |id: &str, limit: usize| {
        let mut args = FluentArgs::new();
        args.set("limit", i64::try_from(limit).unwrap_or(i64::MAX));
        locale.text_with(id, &args)
    };
    let message = match (attribute, code) {
        ("agreement", _) => locale.text("signup-error-agreement"),
        ("username", "ERR_BLANK") => locale.text("signup-error-username-blank"),
        ("username", "ERR_TOO_LONG") => limited(
            "signup-error-username-too-long",
            registration::USERNAME_LENGTH_LIMIT,
        ),
        ("username", "ERR_INVALID") => locale.text("signup-error-username-invalid"),
        ("username", "ERR_TAKEN") => locale.text("signup-error-username-taken"),
        ("username", "ERR_RESERVED") => locale.text("signup-error-username-reserved"),
        ("email", "ERR_INVALID") => locale.text("signup-error-email-invalid"),
        ("email", "ERR_BLOCKED") => locale.text("signup-error-email-blocked"),
        ("email", "ERR_TAKEN") => locale.text("signup-error-email-taken"),
        // The password policy is shared with the reset and change forms.
        ("password", "ERR_BLANK") => locale.text("security-error-password-empty"),
        ("password", "ERR_TOO_SHORT") => {
            limited("security-error-password-short", crate::auth::PASSWORD_MIN)
        }
        ("password", "ERR_TOO_LONG") => {
            limited("security-error-password-long", crate::auth::PASSWORD_MAX)
        }
        ("reason", "ERR_TOO_LONG") => limited(
            "signup-error-reason-too-long",
            registration::REASON_LENGTH_LIMIT,
        ),
        ("date_of_birth", "ERR_BLANK") => locale.text("signup-error-birth-date-blank"),
        ("date_of_birth", "ERR_INVALID") => locale.text("signup-error-birth-date-invalid"),
        _ => return None,
    };
    Some(message)
}

/// Sign-up done but the account sits in the admin approval queue (and no
/// confirmation mail is involved).
async fn pending_approval_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("signup-pending-title")) }
            p { (locale.text("signup-pending-body")) }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("signup-pending-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

async fn check_inbox_page(state: &AppState, email: &str, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("signup-inbox-title")) }
            p {
                (locale.markup(
                    "signup-inbox-body",
                    &[("email", html! { strong { (email) } })],
                ))
            }
            p.settings-field__hint { (locale.text("signup-inbox-hint")) }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("signup-inbox-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

#[derive(Deserialize)]
pub struct ConfirmationQuery {
    confirmation_token: Option<String>,
}

/// `GET /auth/confirmation` — the landing for e-mailed confirmation links
/// (Mastodon's Devise confirmation path, same URL shape).
pub async fn confirm(
    State(state): State<AppState>,
    locale: Locale,
    Query(query): Query<ConfirmationQuery>,
) -> Result<Response, Response> {
    let token = query.confirmation_token.unwrap_or_default();
    if token.is_empty() {
        return Ok(confirmation_page(&state, ConfirmOutcome::Invalid, locale)
            .await
            .into_response());
    }
    let outcome = match registration::confirm_by_token(&state, &token)
        .await
        .map_err(IntoResponse::into_response)?
    {
        Some(user) if user.approved => ConfirmOutcome::Confirmed,
        Some(_) => ConfirmOutcome::PendingApproval,
        None => ConfirmOutcome::Invalid,
    };
    Ok(confirmation_page(&state, outcome, locale)
        .await
        .into_response())
}

#[derive(Clone, Copy)]
enum ConfirmOutcome {
    Confirmed,
    PendingApproval,
    Invalid,
}

async fn confirmation_page(state: &AppState, outcome: ConfirmOutcome, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            @match outcome {
                ConfirmOutcome::Confirmed => {
                    h1 { (locale.text("signup-confirmed-title")) }
                    p { (locale.text("signup-confirmed-body")) }
                    p { a href="/login" { (locale.text("signup-sign-in")) } }
                }
                ConfirmOutcome::PendingApproval => {
                    h1 { (locale.text("signup-confirmed-title")) }
                    p { (locale.text("signup-confirmed-pending-body")) }
                }
                ConfirmOutcome::Invalid => {
                    h1 { (locale.text("signup-confirm-invalid-title")) }
                    p { (locale.text("signup-confirm-invalid-body")) }
                    p { a href="/login" { (locale.text("signup-sign-in")) } }
                }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("signup-confirm-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

fn api_err(err: plamenu_db::DbError) -> Response {
    ApiError::from(err).into_response()
}
