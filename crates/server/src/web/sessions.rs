//! The security page's account-access surfaces: live browser sessions,
//! authorized third-party applications, and the authentication-history log —
//! Mastodon's `/auth/sessions`, `/oauth/authorized_applications` and
//! `/settings/login_activities`, folded onto the one `/settings/security` page.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::login_activity::LoginActivity;
use plamenu_db::oauth::{self, ActiveSession, AuthorizedApp};

use super::clock::{ViewerClock, zone_chip};
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection, ensure_web_app};
use super::settings::redirect_to;
use super::user_agent::describe;
use super::view;
use crate::AppState;
use crate::error::ApiError;

/// How many authentication-history rows to show (Mastodon paginates; the log is
/// bounded here — the recent window is what matters for spotting bad access).
const HISTORY_LIMIT: i64 = 20;

/// The three account-access sections appended to the security page body. Fetched
/// in one place so the page handler stays a straight render.
pub(super) async fn account_access(state: &AppState, user: &WebUser) -> Result<Markup, ApiError> {
    let web_app = ensure_web_app(state).await?;
    let user_id = user.current.user.id;
    let sessions = oauth::list_active_sessions(&state.pool, user_id, web_app.id).await?;
    let apps = oauth::list_authorized_apps(&state.pool, user_id, web_app.id).await?;
    let history =
        plamenu_db::login_activity::list_for_user(&state.pool, user_id, HISTORY_LIMIT).await?;
    let clock = &user.clock;
    let locale = user.locale;
    Ok(html! {
        (zone_chip(clock))
        (sessions_section(user, &sessions, clock, locale))
        (apps_section(user, &apps, clock, locale))
        (history_section(&history, clock, locale))
    })
}

fn sessions_section(
    user: &WebUser,
    sessions: &[ActiveSession],
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    let current = user.current.token_id;
    html! {
        section.settings-form__group {
            h3 { (locale.text("security-sessions-title")) }
            p.settings-field__hint { (locale.text("security-sessions-hint")) }
            (view::data_table(&html! {
                thead {
                    tr {
                        th scope="col" { (locale.text("security-column-browser")) }
                        th scope="col" { (locale.text("security-column-ip")) }
                        th scope="col" { (locale.text("security-column-last-activity")) }
                        th {}
                    }
                }
                tbody {
                    @for session in sessions {
                        tr {
                            td {
                                (describe(session.user_agent.as_deref(), locale))
                                @if session.id == current {
                                    span.admin-badge.is-active {
                                        (locale.text("security-this-device"))
                                    }
                                }
                            }
                            td { samp { (session.last_used_ip.as_deref().unwrap_or("—")) } }
                            td { (activity_cell(session, clock, locale)) }
                            td {
                                @if session.id != current {
                                    form.settings-inline-form method="post"
                                        action=(format!("/web/settings/security/sessions/{}/revoke", session.id)) {
                                        input type="hidden" name="csrf" value=(user.csrf);
                                        button.link-button type="submit" {
                                            (locale.text("security-revoke"))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }))
        }
    }
}

/// The "Last activity" cell: the newest recorded use, or the sign-in time when
/// the session hasn't been used past the touch throttle yet.
fn activity_cell(session: &ActiveSession, clock: &ViewerClock, _locale: Locale) -> Markup {
    let when = session.last_used_at.unwrap_or(session.created_at);
    html! { (clock.element(when)) }
}

fn apps_section(
    user: &WebUser,
    apps: &[AuthorizedApp],
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    html! {
        section.settings-form__group {
            h3 { (locale.text("security-apps-title")) }
            p.settings-field__hint { (locale.text("security-apps-hint")) }
            @if apps.is_empty() {
                p.settings__saved role="status" { (locale.text("security-apps-empty")) }
            } @else {
                (view::data_table(&html! {
                    thead {
                        tr {
                            th scope="col" { (locale.text("security-column-application")) }
                            th scope="col" { (locale.text("security-column-access")) }
                            th scope="col" { (locale.text("security-column-authorized")) }
                            th scope="col" { (locale.text("security-column-last-used")) }
                            th {}
                        }
                    }
                    tbody {
                        @for app in apps {
                            tr {
                                td {
                                    @if let Some(site) = app.website.as_deref().filter(|s| !s.is_empty()) {
                                        a href=(site) target="_blank" rel="noopener noreferrer" { (app.name) }
                                    } @else {
                                        (app.name)
                                    }
                                }
                                td { (scopes_badges(&app.scopes)) }
                                td {
                                    (clock.element_date(app.authorized_at))
                                }
                                td {
                                    @match app.last_used_at {
                                        Some(at) => (clock.element_date(at)),
                                        None => span.admin-table__sub {
                                            (locale.text("security-never"))
                                        },
                                    }
                                }
                                td {
                                    form.settings-inline-form method="post"
                                        action=(format!("/web/settings/security/apps/{}/revoke", app.id)) {
                                        input type="hidden" name="csrf" value=(user.csrf);
                                        button.link-button type="submit" {
                                            (locale.text("security-revoke"))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }))
            }
        }
    }
}

pub(super) fn scopes_badges(scopes: &str) -> Markup {
    html! {
        @for scope in scopes.split_whitespace() {
            span.admin-badge { (scope) }
            " "
        }
    }
}

fn history_section(history: &[LoginActivity], clock: &ViewerClock, locale: Locale) -> Markup {
    html! {
        section.settings-form__group {
            h3 { (locale.text("security-history-title")) }
            p.settings-field__hint { (locale.text("security-history-hint")) }
            @if history.is_empty() {
                p.settings__saved role="status" { (locale.text("security-history-empty")) }
            } @else {
                (view::data_table(&html! {
                    thead {
                        tr {
                            th {}
                            th scope="col" { (locale.text("security-column-method")) }
                            th scope="col" { (locale.text("security-column-ip")) }
                            th scope="col" { (locale.text("security-column-browser")) }
                            th scope="col" { (locale.text("security-column-when")) }
                        }
                    }
                    tbody {
                        @for entry in history {
                            tr {
                                td {
                                    @if entry.success {
                                        span.admin-badge.is-active {
                                            (locale.text("security-attempt-success"))
                                        }
                                    } @else {
                                        span.admin-badge.is-disabled {
                                            (locale.text("security-attempt-failed"))
                                        }
                                    }
                                }
                                td {
                                    (method_label(
                                        entry.authentication_method.as_deref(),
                                        locale,
                                    ))
                                }
                                td { samp { (entry.ip.as_deref().unwrap_or("—")) } }
                                td { (describe(entry.user_agent.as_deref(), locale)) }
                                td { (clock.element(entry.created_at)) }
                            }
                        }
                    }
                }))
            }
        }
    }
}

/// Mastodon's `login_activities.authentication_methods` labels.
fn method_label(method: Option<&str>, locale: Locale) -> String {
    match method {
        Some("password") => locale.text("security-method-password"),
        Some("otp") => locale.text("security-method-otp"),
        Some("webauthn") => locale.text("security-method-webauthn"),
        Some("sign_in_token") => locale.text("security-method-sign-in-token"),
        _ => "—".to_owned(),
    }
}

/// `POST /web/settings/security/sessions/{id}/revoke` — sign one browser session
/// out. The current session can't be revoked here (use log out); an unknown id
/// is a no-op. Either way we return to the security page.
pub async fn revoke_session_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(token_id): Path<i64>,
    body: axum::body::Bytes,
) -> Response {
    if !csrf_ok(&user, &body) {
        return csrf_rejection();
    }
    if token_id == user.current.token_id {
        return redirect_to("/settings/security");
    }
    let web_app = match ensure_web_app(&state).await {
        Ok(app) => app,
        Err(err) => return err.into_response(),
    };
    match oauth::revoke_session(&state.pool, user.current.user.id, web_app.id, token_id).await {
        Ok(_) => redirect_to("/settings/security?saved=session_revoked"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/security/apps/{id}/revoke` — revoke every token the user
/// holds for one application.
pub async fn revoke_app_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(app_id): Path<i64>,
    body: axum::body::Bytes,
) -> Response {
    if !csrf_ok(&user, &body) {
        return csrf_rejection();
    }
    // The first-party web app is never listed as a revocable authorization;
    // refuse to revoke it here so a hand-crafted id can't sign the browser out
    // through this path.
    if let Ok(web_app) = ensure_web_app(&state).await
        && web_app.id == app_id
    {
        return redirect_to("/settings/security");
    }
    match oauth::revoke_app_for_user(&state.pool, user.current.user.id, app_id).await {
        Ok(_) => redirect_to("/settings/security?saved=app_revoked"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Validates the CSRF token from a `application/x-www-form-urlencoded` body.
fn csrf_ok(user: &WebUser, body: &axum::body::Bytes) -> bool {
    #[derive(serde::Deserialize)]
    struct CsrfForm {
        csrf: String,
    }
    serde_urlencoded::from_bytes::<CsrfForm>(body).is_ok_and(|form| user.csrf_ok(&form.csrf))
}
