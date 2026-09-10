//! Scheduled posts — the web half of `/api/v1/scheduled_statuses` (see
//! [`plamenu_db::scheduled_status`] and [`crate::routes::scheduled_statuses`]).
//! Posts are queued from the composer's Schedule field; this page lists the
//! queue and lets the owner reschedule or cancel an entry. The publish sweeper
//! (`crate::scheduled_status_publish`) posts due rows exactly as the API path
//! does.
//!
//! Times are entered and shown as wall-clock readings in the viewer's
//! preference time zone (`users.time_zone`, UTC when unset). Reading goes
//! through [`crate::web::clock::ViewerClock`] like everywhere else; *writing*
//! is what makes this module distinctive — "post this at 9:00" is a wall-clock
//! intent, so a submitted reading is resolved back to an instant through
//! Postgres's tz database ([`plamenu_db::tz`]) rather than in-process.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use fluent_bundle::FluentArgs;
use maud::html;
use plamenu_db::{scheduled_status, tz};
use serde::Deserialize;
use time::{OffsetDateTime, PrimitiveDateTime};

use super::clock::parse_datetime_local;
use super::i18n::Locale;
use super::session::{WebUser, csrf_rejection};
use super::settings::{
    bad_form, error_flash, field, form_pairs, redirect_to, saved_flash, settings_shell,
};
use crate::error::ApiError;
use crate::routes::scheduled_statuses::validate_future;
use crate::state::AppState;

/// Page size, matching the API's ceiling.
const LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct ScheduledQuery {
    max_id: Option<i64>,
    saved: Option<String>,
    error: Option<String>,
}

/// Resolves a submitted `datetime-local` value in the viewer's zone to the
/// instant it names, enforcing the same minimum offset as the API. The shared
/// entry point for the composer's Schedule field and the reschedule form.
pub(super) async fn resolve_schedule_input(
    state: &AppState,
    user: &WebUser,
    raw: &str,
) -> Result<OffsetDateTime, ApiError> {
    let instant = resolve_local_instant(state, user, raw, "Scheduled at").await?;
    validate_future(instant)?;
    Ok(instant)
}

/// A `datetime-local` value read in the viewer's own zone, with no constraint on
/// which side of now it falls.
///
/// Split out of [`resolve_schedule_input`] for the event composer (E4): an
/// event's start is *data*, not a schedule, so a past date is a legitimate thing
/// to record — and an edit to a long-standing event must not start failing merely
/// because the event has since happened. `field` names the field in the 422.
pub(super) async fn resolve_local_instant(
    state: &AppState,
    user: &WebUser,
    raw: &str,
    field: &str,
) -> Result<OffsetDateTime, ApiError> {
    let local = parse_datetime_local(raw)
        .ok_or_else(|| ApiError::Unprocessable(format!("Validation failed: {field} is invalid")))?;
    Ok(tz::local_to_utc(&state.pool, local, user.clock.name()).await?)
}

/// A wall-clock reading as a `datetime-local` input value.
fn input_value(local: PrimitiveDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        local.year(),
        u8::from(local.month()),
        local.day(),
        local.hour(),
        local.minute()
    )
}

/// A wall-clock reading as a human label ("Jul 18, 2026, 09:30"). The zone is
/// already applied, so this is a plain calendar reading with no offset — the
/// catalog supplies the month name and the field order.
fn local_label(local: PrimitiveDateTime, locale: Locale) -> String {
    let mut args = FluentArgs::new();
    args.set(
        "month",
        locale.text(&format!("month-abbr-{}", u8::from(local.month()))),
    );
    args.set("day", local.day());
    args.set("year", local.year());
    args.set(
        "clock",
        format!("{:02}:{:02}", local.hour(), local.minute()),
    );
    locale.text_with("datetime-wall-clock", &args)
}

/// The visibility value as its composer-menu label.
fn visibility_label(visibility: &str, locale: Locale) -> String {
    locale.text(match visibility {
        "unlisted" => "visibility-unlisted",
        "private" => "visibility-private",
        "direct" => "visibility-direct",
        "local" => "visibility-local",
        _ => "visibility-public",
    })
}

/// A short single-line excerpt of the queued text.
fn excerpt(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(120).collect();
    if out.len() < line.len() || text.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// `GET /settings/scheduled` — the viewer's queue, soonest last (the API's
/// newest-id-first order), with reschedule and cancel on each entry.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<ScheduledQuery>,
) -> Response {
    let account_id = user.current.account.id;
    let rows = match scheduled_status::list_for_account(
        &state.pool,
        account_id,
        query.max_id,
        None,
        None,
        LIMIT,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let zone = user.clock.name();
    let instants: Vec<OffsetDateTime> = rows.iter().map(|row| row.scheduled_at).collect();
    let wall_clocks = match tz::utc_to_local(&state.pool, &instants, zone).await {
        Ok(readings) => readings,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let full = rows.len() >= usize::try_from(LIMIT).unwrap_or(usize::MAX);
    let next = full
        .then(|| rows.last().map(|row| row.id))
        .flatten()
        .map(|id| format!("/settings/scheduled?max_id={id}"));
    let locale = user.locale;
    // Two wordings rather than a conditional fragment: only a zone the viewer
    // actually chose is worth calling their preference.
    let zone_hint = {
        let wording = if zone == "UTC" {
            "scheduled-times-hint"
        } else {
            "scheduled-times-hint-preference"
        };
        locale.markup(
            wording,
            &[
                ("zone", html! { (user.clock.label()) }),
                (
                    "preferences",
                    html! { a href="/settings/preferences" {
                        (locale.text("scheduled-preferences-link"))
                    } },
                ),
            ],
        )
    };
    let body = html! {
        (saved_flash(
            query.saved.as_deref() == Some("scheduled"),
            &locale.text("scheduled-posted"),
        ))
        (saved_flash(
            query.saved.as_deref().is_some_and(|v| v != "scheduled"),
            &locale.text("scheduled-updated"),
        ))
        (error_flash(query.error.as_deref()))
        p.settings-field__hint { (zone_hint) }
        @if rows.is_empty() {
            p.empty {
                (locale.markup("scheduled-empty", &[("composer", html! {
                    a href="/compose" { (locale.text("scheduled-composer-link")) }
                })]))
            }
        } @else {
            ul.list-index {
                @for (row, local) in rows.iter().zip(&wall_clocks) {
                    (queue_entry(row, *local, &user.csrf, locale))
                }
            }
        }
        @if let Some(href) = &next {
            nav.pager { a.pager__more href=(href) { (locale.text("page-load-more")) } }
        }
    };
    settings_shell(
        &user,
        "/settings/scheduled",
        &locale.text("scheduled-title"),
        &body,
    )
    .into_response()
}

/// One queued entry: its wall-clock time, a one-line summary of what will be
/// published, and the reschedule/cancel forms.
fn queue_entry(
    row: &scheduled_status::ScheduledStatus,
    local: PrimitiveDateTime,
    csrf: &str,
    locale: Locale,
) -> maud::Markup {
    // The summary reads as one localized clause per fact, joined with the
    // separator the surrounding list already uses.
    let mut facts = vec![visibility_label(&row.visibility, locale)];
    if row.object_type == "Article" {
        facts.push(locale.text("compose-kind-article"));
    }
    if !row.spoiler_text.is_empty() {
        let mut args = FluentArgs::new();
        args.set("text", excerpt(&row.spoiler_text));
        facts.push(locale.text_with("scheduled-content-warning", &args));
    }
    if !row.media_ids.is_empty() {
        let mut args = FluentArgs::new();
        args.set(
            "count",
            i64::try_from(row.media_ids.len()).unwrap_or(i64::MAX),
        );
        facts.push(locale.text_with("scheduled-attachments", &args));
    }
    if row.poll_options.as_ref().is_some_and(|o| !o.is_empty()) {
        facts.push(locale.text("scheduled-poll"));
    }
    html! {
        li.list-index__item {
            strong { (local_label(local, locale)) }
            span.settings-field__hint { (facts.join(" · ")) }
            @if let Some(title) = &row.title { p { strong { (title) } } }
            @if !row.text.is_empty() { p { (excerpt(&row.text)) } }
            form.settings-form--inline method="post"
                action=(format!("/web/settings/scheduled/{}/reschedule", row.id)) {
                input type="hidden" name="csrf" value=(csrf);
                label.compose__inline {
                    span.visually-hidden { (locale.text("scheduled-new-time")) }
                    input type="datetime-local" name="scheduled_at"
                        value=(input_value(local)) required;
                }
                button type="submit" { (locale.text("scheduled-reschedule")) }
            }
            form method="post"
                action=(format!("/web/settings/scheduled/{}/cancel", row.id)) {
                input type="hidden" name="csrf" value=(csrf);
                button.settings-button--danger type="submit" {
                    (locale.text("scheduled-cancel"))
                }
            }
        }
    }
}

/// `POST /web/settings/scheduled/{id}/reschedule` — move an entry to a new
/// time (the only edit Mastodon permits on a scheduled status).
pub async fn reschedule_action(
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
    let raw = field(&pairs, "scheduled_at").unwrap_or_default();
    let instant = match resolve_schedule_input(&state, &user, raw).await {
        Ok(instant) => instant,
        Err(ApiError::Unprocessable(message)) => {
            return redirect_error("/settings/scheduled", &message);
        }
        Err(err) => return err.into_response(),
    };
    match scheduled_status::update_scheduled_at(&state.pool, user.current.account.id, id, instant)
        .await
    {
        // A row already published or cancelled in another tab is gone — the
        // refreshed listing says so better than a 404 page.
        Ok(_) => redirect_to("/settings/scheduled?saved=1"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `POST /web/settings/scheduled/{id}/cancel` — drop an entry from the queue.
pub async fn cancel_action(
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
    match scheduled_status::delete(&state.pool, user.current.account.id, id).await {
        Ok(_) => redirect_to("/settings/scheduled?saved=1"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Redirects back to the listing with an error flash.
fn redirect_error(base: &str, message: &str) -> Response {
    let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
    redirect_to(&format!("{base}?{query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_local_parses_with_and_without_seconds() {
        let parsed = parse_datetime_local("2026-07-18T09:30").unwrap();
        assert_eq!(input_value(parsed), "2026-07-18T09:30");
        let with_seconds = parse_datetime_local("2026-07-18T09:30:45").unwrap();
        assert_eq!(with_seconds.second(), 45);
        assert!(parse_datetime_local("2026-07-18").is_none());
        assert!(parse_datetime_local("2026-13-01T00:00").is_none());
        assert!(parse_datetime_local("").is_none());
    }

    #[test]
    fn excerpt_truncates_and_marks_continuation() {
        assert_eq!(excerpt("short"), "short");
        assert_eq!(excerpt("first line\nsecond"), "first line…");
        let long = "x".repeat(200);
        assert_eq!(excerpt(&long).chars().count(), 121);
    }
}
