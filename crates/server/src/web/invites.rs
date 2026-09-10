//! Invite management — Mastodon's `/invites` page, as a settings
//! section: mint shareable sign-up codes, watch their use, deactivate them.
//! Access follows the `invite_users` permission of the viewer's assigned
//! role; the default "User" role ships without it (migration 0015), so
//! inviting is an explicit admin grant.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::invite::{self, Invite, NewInvite};
use serde::Deserialize;

use super::clock::ViewerClock;
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, error_flash, field, form_pairs, redirect_to, settings_shell};
use super::view;
use crate::error::ApiError;
use crate::state::AppState;

/// Mastodon's invite-expiry choices (seconds), plus "never". The catalog keys
/// are shared with the filter form's identical choices.
const EXPIRY_CHOICES: &[(i64, &str)] = &[
    (1_800, "filters-expiry-30-minutes"),
    (3_600, "filters-expiry-1-hour"),
    (21_600, "filters-expiry-6-hours"),
    (43_200, "filters-expiry-12-hours"),
    (86_400, "filters-expiry-1-day"),
    (604_800, "filters-expiry-1-week"),
];
const MAX_USES_CHOICES: &[i32] = &[1, 5, 10, 25, 50, 100];
const COMMENT_LENGTH_LIMIT: usize = 420;

/// Mastodon's `Invite#generate_code`: 8 alphanumeric characters. Rejection
/// sampling keeps the distribution uniform.
fn generate_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    // The largest multiple of 62 below 256, so `% 62` stays unbiased.
    const LIMIT: u8 = 248;
    let mut code = String::with_capacity(8);
    while code.len() < 8 {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("OS entropy source failed");
        for byte in bytes {
            if byte < LIMIT && code.len() < 8 {
                code.push(ALPHABET[usize::from(byte) % ALPHABET.len()] as char);
            }
        }
    }
    code
}

// The `Err` is an axum `Response` (large by nature); the matching handlers
// return `Result<Response, Response>` where both variants are that size, so
// boxing here would only add noise.
#[allow(clippy::result_large_err)]
fn require_invite_permission(user: &WebUser) -> Result<(), Response> {
    if user.can_invite {
        Ok(())
    } else {
        Err(ApiError::Forbidden("This action is not allowed".into()).into_response())
    }
}

#[derive(Deserialize)]
pub struct InvitesQuery {
    error: Option<String>,
}

/// `GET /settings/invites`.
pub async fn page(State(state): State<AppState>, user: WebUser) -> Result<Response, Response> {
    require_invite_permission(&user)?;
    let invites = invite::list_by_user(&state.pool, user.current.user.id)
        .await
        .map_err(|err| ApiError::from(err).into_response())?;
    let clock = &user.clock;
    let locale = user.locale;
    let body = html! {
        p {
            a.pill-button href="/settings/invites/new" { (locale.text("invites-new")) }
        }

        @if invites.is_empty() {
            p.settings-field__hint { (locale.text("invites-empty")) }
        } @else {
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("invites-column-link")) }
                        th scope="col" { (locale.text("invites-column-uses")) }
                        th scope="col" { (locale.text("invites-column-expires")) }
                        th scope="col" { (locale.text("invites-column-comment")) }
                        th scope="col" { "" }
                    }
                }
                tbody {
                    @for invite in &invites {
                        (invite_row(&state, &user, invite, clock, locale))
                    }
                }
            }))
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/invites",
        &locale.text("invites-title"),
        &body,
    )
    .into_response())
}

fn invite_row(
    state: &AppState,
    user: &WebUser,
    invite: &Invite,
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    let link = format!("https://{}/invite/{}", state.config.domain, invite.code);
    let usable = invite.valid_for_use();
    html! {
        tr {
            td {
                @if usable {
                    a href=(link) { code { (invite.code) } }
                } @else {
                    s { code { (invite.code) } }
                }
            }
            td {
                (invite.uses)
                @if let Some(max) = invite.max_uses { " / " (max) }
            }
            td {
                @match invite.expires_at {
                    Some(at) => (clock.element(at)),
                    None => (locale.text("invites-never-expires")),
                }
            }
            td { (invite.comment) }
            td {
                @if usable {
                    form method="post"
                        action=(format!("/web/settings/invites/{}/expire", invite.id)) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        button.settings-button--danger type="submit" {
                            (locale.text("invites-deactivate"))
                        }
                    }
                }
            }
        }
    }
}

/// `GET /settings/invites/new` — the dedicated invite-creation page.
pub async fn new_page(
    user: WebUser,
    Query(query): Query<InvitesQuery>,
) -> Result<Response, Response> {
    require_invite_permission(&user)?;
    let locale = user.locale;
    let error = match query.error.as_deref() {
        Some("comment_too_long") => Some(locale.text("invites-error-comment-too-long")),
        Some(_) => Some(locale.text("invites-error-failed")),
        None => None,
    };
    let uses_label = |uses: i32| {
        let mut args = FluentArgs::new();
        args.set("count", uses);
        locale.text_with("invites-uses-count", &args)
    };
    let body = html! {
        (error_flash(error.as_deref()))

        form.settings-form method="post" action="/web/settings/invites" {
            input type="hidden" name="csrf" value=(user.csrf);
            fieldset.settings-form__group {
                legend { (locale.text("invites-new")) }
                label.settings-field {
                    span.settings-field__label { (locale.text("invites-max-uses")) }
                    select name="max_uses" {
                        option value="" selected { (locale.text("invites-no-limit")) }
                        @for uses in MAX_USES_CHOICES {
                            option value=(uses) { (uses_label(*uses)) }
                        }
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("filters-field-expiry")) }
                    select name="expires_in" {
                        option value="" selected { (locale.text("invites-never-expires")) }
                        @for (seconds, message) in EXPIRY_CHOICES {
                            option value=(seconds) { (locale.text(message)) }
                        }
                    }
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("invites-comment")) }
                    input type="text" name="comment" maxlength="420"
                        placeholder=(locale.text("invites-comment-placeholder"));
                    span.settings-field__hint { (locale.text("invites-comment-hint")) }
                }
            }
            div.settings-form__actions {
                button type="submit" { (locale.text("invites-generate")) }
                a.settings-button--plain href="/settings/invites" {
                    (locale.text("filters-cancel"))
                }
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/invites",
        &locale.text("invites-new"),
        &body,
    )
    .into_response())
}

/// `POST /web/settings/invites` — mint a code.
pub async fn create_action(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if let Err(response) = require_invite_permission(&user) {
        return response;
    }
    let comment = field(&pairs, "comment").unwrap_or_default().trim();
    if comment.chars().count() > COMMENT_LENGTH_LIMIT {
        return redirect_to("/settings/invites/new?error=comment_too_long");
    }
    let max_uses = field(&pairs, "max_uses")
        .and_then(|v| v.parse::<i32>().ok())
        .filter(|&n| n > 0);
    let expires_in = field(&pairs, "expires_in")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&s| s > 0);
    match invite::create(
        &state.pool,
        NewInvite {
            user_id: user.current.user.id,
            code: &generate_code(),
            expires_in,
            max_uses,
            comment,
        },
    )
    .await
    {
        Ok(_) => redirect_to("/settings/invites"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/invites/{id}/expire` — deactivate one of your codes.
pub async fn expire_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    if let Err(response) = require_invite_permission(&user) {
        return response;
    }
    // The user_id guard means expiring someone else's invite is a no-op.
    match invite::expire(&state.pool, id, user.current.user.id).await {
        Ok(_) => redirect_to("/settings/invites"),
        Err(err) => ApiError::from(err).into_response(),
    }
}
