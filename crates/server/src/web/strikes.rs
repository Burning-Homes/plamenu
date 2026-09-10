//! The user's own moderation strikes and the appeal-submission flow
//! (Mastodon's `/disputes/strikes` pages). Each strike may be appealed
//! once, within [`appeal::APPEAL_WINDOW_DAYS`] of being issued; decisions
//! arrive from the admin queue at `/admin/appeals`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::{Markup, html};
use plamenu_db::account_warning::{self, AccountWarning};
use plamenu_db::appeal::{self, Appeal};
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use super::clock::ViewerClock;
use super::i18n::Locale;
use super::session::WebUser;
use super::settings::{SettingsQuery, settings_shell};
use crate::AppState;
use crate::web::session::csrf_rejection;

/// What a strike verb did, in the user's terms.
fn action_label(action: &str, locale: Locale) -> String {
    let id = match action {
        "disable" => "strikes-action-disable",
        "sensitive" => "strikes-action-sensitive",
        "silence" => "strikes-action-silence",
        "suspend" => "strikes-action-suspend",
        _ => "strikes-action-warning",
    };
    locale.text(id)
}

fn within_window(strike: &AccountWarning) -> bool {
    strike.created_at >= OffsetDateTime::now_utc() - Duration::days(appeal::APPEAL_WINDOW_DAYS)
}

/// `GET /settings/strikes` — the user's strike history with appeal forms.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Response, Response> {
    let account_id = user.current.account.id;
    let strikes = account_warning::for_target(&state.pool, account_id)
        .await
        .map_err(api_err)?;
    let appeals = appeal::for_account(&state.pool, account_id)
        .await
        .map_err(api_err)?;
    let clock = &user.clock;
    let locale = user.locale;
    let csrf = user.csrf.as_str();
    let mut window = FluentArgs::new();
    window.set("days", appeal::APPEAL_WINDOW_DAYS);
    let body = html! {
        @if query.saved.is_some() {
            p.admin-flash role="status" { (locale.text("strikes-appeal-saved")) }
        }
        @if query.error.is_some() {
            p.admin-flash.is-error role="alert" { (locale.text("strikes-appeal-error")) }
        }
        @if strikes.is_empty() {
            p.muted { (locale.text("strikes-empty")) }
        } @else {
            p.muted { (locale.text_with("strikes-intro", &window)) }
            @for strike in &strikes {
                (strike_row(
                    strike,
                    appeals.iter().find(|a| a.account_warning_id == strike.id),
                    csrf,
                    clock,
                    locale,
                ))
            }
        }
    };
    Ok(settings_shell(
        &user,
        "/settings/strikes",
        &locale.text("strikes-title"),
        &body,
    )
    .into_response())
}

fn strike_row(
    strike: &AccountWarning,
    filed: Option<&Appeal>,
    csrf: &str,
    clock: &ViewerClock,
    locale: Locale,
) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong { (action_label(&strike.action, locale)) }
                span.admin-table__sub {
                    (clock.element_date(strike.created_at))
                }
            }
            @if !strike.text.is_empty() {
                p.admin-note__body { (strike.text) }
            }
            @match filed {
                Some(appeal) => {
                    p.admin-table__sub {
                        @if appeal.approved_at.is_some() {
                            (locale.text("strikes-appeal-approved"))
                        } @else if appeal.rejected_at.is_some() {
                            (locale.text("strikes-appeal-rejected"))
                        } @else {
                            (locale.text("strikes-appeal-pending"))
                        }
                    }
                }
                None => {
                    @if within_window(strike) {
                        details {
                            summary { (locale.text("strikes-appeal-open")) }
                            form.settings-form method="post"
                                action=(format!("/web/settings/strikes/{}/appeal", strike.id)) {
                                input type="hidden" name="csrf" value=(csrf);
                                label {
                                    (locale.text("strikes-appeal-label"))
                                    textarea name="text" rows="4"
                                        maxlength=(appeal::TEXT_LENGTH_LIMIT) required {}
                                }
                                button type="submit" { (locale.text("strikes-appeal-submit")) }
                            }
                        }
                    } @else {
                        p.admin-table__sub { (locale.text("strikes-appeal-closed")) }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AppealForm {
    csrf: String,
    #[serde(default)]
    text: String,
}

/// `POST /web/settings/strikes/{id}/appeal` — file the one-shot appeal.
pub async fn submit_appeal(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<AppealForm>,
) -> Result<Response, Response> {
    if !user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let text = form.text.trim();
    if text.is_empty() || text.chars().count() > appeal::TEXT_LENGTH_LIMIT {
        return Ok(redirect_strikes("error"));
    }
    let account_id = user.current.account.id;
    let Some(strike) = account_warning::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_strikes("error"));
    };
    // Only your own, recent strikes are appealable.
    if strike.target_account_id != account_id || !within_window(&strike) {
        return Ok(redirect_strikes("error"));
    }
    // The unique index rejects a second appeal of the same strike.
    if appeal::create(&state.pool, account_id, strike.id, text)
        .await
        .is_err()
    {
        return Ok(redirect_strikes("error"));
    }
    Ok(redirect_strikes("saved"))
}

fn redirect_strikes(outcome: &str) -> Response {
    let target = match outcome {
        "saved" => "/settings/strikes?saved=1",
        _ => "/settings/strikes?error=1",
    };
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, target.to_owned())],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
