//! Self-service password reset — the web counterpart of Mastodon's
//! Devise `passwords` views, at the same URL shapes: `/auth/password/new`
//! requests the mail and the e-mailed link lands on
//! `/auth/password/edit?reset_password_token=…`. A successful reset revokes
//! every live token of the user (sessions and API clients alike), matching
//! Mastodon's sign-out-everywhere behavior.

use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::user;
use serde::Deserialize;

use plamenu_db::email::OutgoingEmail;
use plamenu_db::instance_settings;

use super::i18n::Locale;
use super::layout;
use super::session::MaybeWebUser;
use crate::error::ApiError;
use crate::state::AppState;
use crate::{auth, mailer};

/// `GET /auth/password/new` — the "forgot your password" request form.
/// Signed-in visitors are bounced home.
pub async fn request_form(
    State(state): State<AppState>,
    locale: Locale,
    MaybeWebUser(session): MaybeWebUser,
) -> Response {
    if session.is_some() {
        return Redirect::to("/").into_response();
    }
    if !mailer::enabled(&state) {
        return unavailable_page(&state, locale).await.into_response();
    }
    request_page(&state, locale).await.into_response()
}

async fn unavailable_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-unavailable-title")) }
            p { (locale.text("password-reset-unavailable-body")) }
            p { a href="/login" { (locale.text("password-reset-back")) } }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-unavailable-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

async fn request_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-title")) }
            p { (locale.text("password-reset-intro")) }
            form.auth-form method="post" action="/auth/password" {
                label {
                    (locale.text("password-reset-email"))
                    input type="email" name="email" autocomplete="email"
                        required autofocus;
                }
                button type="submit" { (locale.text("password-reset-send")) }
            }
            p.auth-card__alt { a href="/login" { (locale.text("password-reset-back")) } }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

#[derive(Deserialize)]
pub struct RequestForm {
    #[serde(default)]
    email: String,
}

/// `POST /auth/password` — enqueue the reset mail. Always answers with the
/// same page whether or not the address exists (Devise's paranoid mode), so
/// the form cannot be used to enumerate accounts.
pub async fn request_submit(
    State(state): State<AppState>,
    locale: Locale,
    Form(form): Form<RequestForm>,
) -> Result<Response, Response> {
    if !mailer::enabled(&state) {
        return Ok(unavailable_page(&state, locale).await.into_response());
    }
    let email = form.email.trim();
    // Mastodon's `throttle_password_resets/email`.
    crate::rate_limit::check_email(
        &state,
        crate::rate_limit::Bucket::PasswordResetsEmail,
        email,
    )
    .await
    .map_err(IntoResponse::into_response)?;
    // Response latency is decoupled from account existence: the
    // lookup, token write and durable mail enqueue all run in a background
    // task, so a known and an unknown address do identical work — the rate
    // check above and the page below — before the identical response. The
    // paranoid-mode page reveals nothing either way, so a task failure is
    // logged rather than surfaced (the durable #46 atomicity inside the task
    // still holds: token and mail job commit together or not at all).
    let task_state = state.clone();
    let address = email.to_owned();
    tokio::spawn(async move {
        let user = match user::find_by_email(&task_state.pool, &address).await {
            Ok(user) => user,
            Err(error) => {
                tracing::error!(%error, "password reset lookup failed");
                return;
            }
        };
        // The lookup matched on the address, so it is always present here.
        if let Some(user) = user
            && let Some(recipient) = user.email.clone()
        {
            let token = auth::generate_secret();
            if let Err(error) =
                store_reset_token_and_send_email(&task_state, user.id, &recipient, &token).await
            {
                tracing::error!(%error, "password reset token/mail enqueue failed");
            }
        }
    });
    Ok(sent_page(&state, email, locale).await.into_response())
}

/// Renders the reset instructions and, atomically, stores the fresh reset token
/// alongside the queued mail: the mail is rendered
/// first, then the token update and the `email_jobs` insert commit as one unit,
/// so a failed enqueue rolls back the token and any previous reset link stays
/// valid. Shared by the self-service request and the admin reset.
pub(crate) async fn store_reset_token_and_send_email(
    state: &AppState,
    user_id: i64,
    recipient: &str,
    token: &str,
) -> Result<(), ApiError> {
    let locale = Locale::for_user(&state.pool, user_id).await?;
    let (subject, body) = render_reset_email(state, token, locale).await?;
    user::set_reset_password_token_with_mail(
        &state.pool,
        user_id,
        &auth::hash_secret(token),
        &OutgoingEmail {
            recipient,
            subject: &subject,
            body: &body,
        },
    )
    .await?;
    Ok(())
}

/// Renders the reset instructions (subject, plain-text body) without mutating
/// any credential — the "render first" half of the atomic reset transition.
/// Shared with the admin reset (`op_reset_password`), which renders the mail
/// before committing its own credential transition.
pub(crate) async fn render_reset_email(
    state: &AppState,
    token: &str,
    locale: Locale,
) -> Result<(String, String), ApiError> {
    let settings = instance_settings::get(&state.pool).await?;
    let domain = &state.config.domain;
    let link = format!("https://{domain}/auth/password/edit?reset_password_token={token}");
    let mut args = FluentArgs::new();
    args.set("site", settings.site_title.clone());
    let subject = locale.plain_with("email-reset-subject", &args);
    args.set("url", format!("https://{domain}"));
    // Plain-text mail, so the paragraphs are separate messages joined here —
    // each one is still a whole sentence for the translator, and nothing but
    // the link is assembled from fragments.
    let body = format!(
        "{greeting}\n\n{intro}\n\n{hint}\n\n{link}\n\n{ignore}\n",
        greeting = locale.plain("email-reset-greeting"),
        intro = locale.plain_with("email-reset-intro", &args),
        hint = locale.plain("email-reset-link-hint"),
        ignore = locale.plain("email-reset-ignore"),
    );
    Ok((subject, body))
}

async fn sent_page(state: &AppState, email: &str, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-sent-title")) }
            p {
                (locale.markup(
                    "password-reset-sent-body",
                    &[("email", html! { strong { (email) } })],
                ))
            }
            p { a href="/login" { (locale.text("password-reset-back")) } }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-sent-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

#[derive(Deserialize)]
pub struct EditQuery {
    reset_password_token: Option<String>,
}

/// `GET /auth/password/edit?reset_password_token=` — the e-mailed link's
/// landing: the new-password form, or the invalid-link notice.
pub async fn edit_form(
    State(state): State<AppState>,
    locale: Locale,
    Query(query): Query<EditQuery>,
) -> Result<Response, Response> {
    let token = query.reset_password_token.unwrap_or_default();
    let live = !token.is_empty()
        && user::find_by_reset_password_token_hash(&state.pool, &auth::hash_secret(&token))
            .await
            .map_err(api_err)?
            .is_some();
    if !live {
        return Ok(invalid_page(&state, locale).await.into_response());
    }
    Ok(edit_page(&state, &token, &[], locale).await.into_response())
}

async fn invalid_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-invalid-title")) }
            p { (locale.text("password-reset-invalid-body")) }
            p {
                a href="/auth/password/new" {
                    (locale.text("password-reset-invalid-link"))
                }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-invalid-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

async fn edit_page(state: &AppState, token: &str, errors: &[String], locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-choose-title")) }
            @if !errors.is_empty() {
                ul.form-error id="password-reset-errors" role="alert" {
                    @for error in errors { li { (error) } }
                }
            }
            form.auth-form method="post" action="/auth/password/edit" {
                input type="hidden" name="reset_password_token" value=(token);
                label {
                    (locale.text("password-reset-new"))
                    input type="password" name="password"
                        autocomplete="new-password" minlength=(auth::PASSWORD_MIN)
                        maxlength=(auth::PASSWORD_MAX) required autofocus
                        aria-invalid=[(!errors.is_empty()).then_some("true")]
                        aria-describedby=[(!errors.is_empty()).then_some("password-reset-errors")];
                }
                label {
                    (locale.text("password-reset-confirm"))
                    input type="password" name="password_confirmation"
                        autocomplete="new-password" minlength=(auth::PASSWORD_MIN)
                        maxlength=(auth::PASSWORD_MAX) required
                        aria-invalid=[(!errors.is_empty()).then_some("true")]
                        aria-describedby=[(!errors.is_empty()).then_some("password-reset-errors")];
                }
                button type="submit" { (locale.text("password-reset-change")) }
            }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-choose-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

#[derive(Deserialize)]
pub struct EditForm {
    #[serde(default)]
    reset_password_token: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password_confirmation: String,
}

/// `POST /auth/password/edit` — set the new password behind a live token,
/// then sign the user out everywhere.
pub async fn edit_submit(
    State(state): State<AppState>,
    locale: Locale,
    Form(form): Form<EditForm>,
) -> Result<Response, Response> {
    let token = form.reset_password_token;
    if token.is_empty() {
        return Ok(invalid_page(&state, locale).await.into_response());
    }

    let mut errors = Vec::new();
    if form.password != form.password_confirmation {
        errors.push(locale.text("security-error-password-mismatch"));
    }
    // One shared length policy, rendered through the same
    // catalog mapping the signed-in change form uses, so neither form can
    // quote a stale limit.
    if let Err(policy) = auth::validate_password(&form.password) {
        errors.push(super::two_factor::password_policy_message(policy, locale));
    }
    if !errors.is_empty() {
        let page = edit_page(&state, &token, &errors, locale).await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
    }

    // Validate the token is real and live *before* spending an Argon2 hash, so
    // an invented token can no longer force expensive crypto on this anonymous
    // route. This POST is also admission-limited per IP by the
    // rate-limit middleware (it now classifies as a login-attempt path). The
    // atomic claim below re-checks the token under a single UPDATE, so this
    // cheap pre-filter never widens the check→claim race — it only keeps bogus
    // tokens out of the hash.
    let token_hash = auth::hash_secret(&token);
    if user::find_by_reset_password_token_hash(&state.pool, &token_hash)
        .await
        .map_err(api_err)?
        .is_none()
    {
        return Ok(invalid_page(&state, locale).await.into_response());
    }
    let password_hash = auth::hash_password_gated(form.password.clone())
        .await
        .map_err(IntoResponse::into_response)?;
    // Set the password behind the live token *and* sign the account out
    // everywhere in one transaction: the password change, the
    // single-use token consumption, and the sign-out-everywhere revocation now
    // commit or roll back together, so a self-service reset can no longer change
    // the password and burn the link while leaving a copied token live.
    let reset = user::reset_password_by_token_and_revoke(&state.pool, &token_hash, &password_hash)
        .await
        .map_err(api_err)?;
    if reset.is_none() {
        return Ok(invalid_page(&state, locale).await.into_response());
    }

    Ok(done_page(&state, locale).await.into_response())
}

async fn done_page(state: &AppState, locale: Locale) -> Markup {
    let content = html! {
        section.auth-card {
            h1 { (locale.text("password-reset-done-title")) }
            p { (locale.text("password-reset-done-body")) }
            p { a href="/login" { (locale.text("password-reset-done-link")) } }
        }
    };
    layout::shell_visitor_localized(
        &locale.text("password-reset-done-title"),
        None,
        super::pages::anon_nav(state).await,
        &content,
        locale,
    )
}

fn api_err(err: plamenu_db::DbError) -> Response {
    ApiError::from(err).into_response()
}
