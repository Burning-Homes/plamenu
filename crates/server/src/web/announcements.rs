//! Server announcements, user side — the web half of
//! `/api/v1/announcements` (see [`crate::routes::announcements`]). Unread
//! announcements surface as a banner on the home timeline; `/announcements`
//! lists every published one, read or not. Both render the same entity the
//! API serves, with emoji-reaction chips (shared classes with status
//! reactions), a progressively enhanced reaction picker and a dismiss verb.
//! Admins write announcements in the admin
//! console; nothing here can create one.

use std::future::Future;

use axum::body::Bytes;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::announcement;
use serde_json::Value;

use super::actions::safe_return;
use super::clock::ViewerClock;
use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, field, form_pairs, redirect_to};
use super::{layout, reactions, view};
use crate::error::ApiError;
use crate::routes::announcements as api;
use crate::state::AppState;

/// One announcement card: content with custom emoji applied, the cited
/// statuses (`status_ids`) as embedded cards, the reaction chips (toggle
/// forms, like status reactions), the progressively enhanced reaction picker
/// and — while
/// unread — the dismiss verb.
fn card(ann: &Value, user: &WebUser, return_to: &str, clock: &ViewerClock) -> Markup {
    let id = ann.get("id").and_then(Value::as_str).unwrap_or_default();
    let base = format!("/web/announcements/{id}");
    let content = ann
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty = Vec::new();
    let emojis = ann
        .get("emojis")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let reactions = ann
        .get("reactions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let statuses = ann
        .get("statuses")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let read = ann.get("read").and_then(Value::as_bool).unwrap_or(false);
    let published = ann
        .get("published_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    html! {
        article.announcement.is-unread[!read] data-announcement=(id) {
            header.announcement__head {
                span.announcement__title {
                    (view::icon("bell")) " " (user.locale.text("announcements-card-title"))
                    @if !read {
                        span.announcement__unread { (user.locale.text("announcements-unread")) }
                    }
                }
                @if !published.is_empty() {
                    (clock.element_iso(published))
                }
            }
            div.status__content { (view::emojify(content, emojis)) }
            @for status in statuses {
                (view::cited_status_card(&view::Status(status), true))
            }
            div.announcement__foot {
                (view::reaction_chips_row(
                    &base, reactions, Some(&user.csrf), return_to, user.locale))
                (view::reaction_picker(&base, &user.csrf, return_to, user.locale))
                @if !read {
                    form method="post" action=(format!("{base}/dismiss"))
                        data-announcement-dismiss
                        data-dismiss-failed=(user.locale.text("announcements-dismiss-failed")) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        input type="hidden" name="return_to" value=(return_to);
                        button type="submit" { (user.locale.text("announcements-mark-read")) }
                    }
                }
            }
        }
    }
}

/// The home-timeline banner: every *unread* announcement as a card, plus a
/// link to the full listing when any announcement exists at all. Renders
/// nothing when the instance has never published one — the common case, kept
/// to one cheap query.
pub(super) async fn home_banner(state: &AppState, user: &WebUser) -> Result<Markup, ApiError> {
    let all = api::announcements_json(state, user.current.account.id).await?;
    if all.is_empty() {
        return Ok(html! {});
    }
    let unread: Vec<&Value> = all
        .iter()
        .filter(|a| !a.get("read").and_then(Value::as_bool).unwrap_or(false))
        .collect();
    let clock = &user.clock;
    Ok(html! {
        @if !unread.is_empty() {
            section.announcements-banner data-announcements-banner
                data-empty-label=(user.locale.text("announcements-title")) {
                @for ann in &unread { (card(ann, user, "/", clock)) }
            }
        } @else {
            p.announcements-link {
                a href="/announcements" {
                    (view::icon("bell")) " " (user.locale.text("announcements-title"))
                }
            }
        }
    })
}

/// `GET /announcements` — every published announcement, newest last
/// (chronological, like the API).
pub async fn page(State(state): State<AppState>, user: WebUser) -> Response {
    let all = match api::announcements_json(&state, user.current.account.id).await {
        Ok(all) => all,
        Err(err) => return err.into_response(),
    };
    let clock = &user.clock;
    let title = user.locale.text("announcements-title");
    let body = html! {
        section.column {
            h1 { (view::icon("bell")) " " (title) }
            @if all.is_empty() {
                p.empty { (user.locale.text("announcements-empty")) }
            } @else {
                @for ann in &all { (card(ann, &user, "/announcements", clock)) }
            }
        }
    };
    layout::shell(&title, Some(&user), &body).into_response()
}

/// Shared verb wrapper: CSRF, run, bounce back to where the form was. `verb`
/// is a lazy future — nothing runs unless the token checks out.
async fn run_verb(
    user: &WebUser,
    body: &Bytes,
    verb: impl Future<Output = Result<(), ApiError>>,
) -> Response {
    let pairs = match form_pairs(body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let back = safe_return(field(&pairs, "return_to"), "/announcements");
    match verb.await {
        // An announcement unpublished mid-flight (404) just refreshes the view.
        Ok(()) | Err(ApiError::NotFound) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `POST /web/announcements/{id}/react/{name}` — toggle on an existing chip.
pub async fn react_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, name)): Path<(i64, String)>,
    body: Bytes,
) -> Response {
    let account_id = user.current.account.id;
    run_verb(
        &user,
        &body,
        api::add_reaction(&state, account_id, id, &name),
    )
    .await
}

/// `GET /web/announcements/{id}/reaction` — the on-demand complete picker used
/// by the plain link when JavaScript is unavailable.
pub async fn reaction_picker_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<super::pages::FragmentQuery>,
) -> Result<Response, ApiError> {
    if announcement::find_published(&state.pool, id)
        .await?
        .is_none()
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    reactions::picker_page(
        &state,
        &user,
        &format!("/web/announcements/{id}"),
        query.return_to.as_deref(),
        "/announcements",
    )
    .await
}

/// `POST /web/announcements/{id}/react` — applies a choice from the complete
/// no-JavaScript picker and returns to the page that opened it.
pub async fn react_selected_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<reactions::PickerForm>,
) -> Response {
    if !user.csrf_ok(&form.csrf) {
        return csrf_rejection();
    }
    let back = safe_return(form.return_to.as_deref(), "/announcements");
    match api::add_reaction(&state, user.current.account.id, id, &form.emoji).await {
        Ok(()) | Err(ApiError::NotFound) => redirect_to(&back),
        Err(err) => err.into_response(),
    }
}

/// `GET /web/announcements/{id}/reactions` — the reaction-chip row on its
/// own, fetched by the script to swap the row in place after a fetch-based
/// react/unreact (the announcement face of the status fragment in
/// [`super::pages::reactions_fragment`]). The no-JS path never hits this: its
/// plain form POST re-renders the whole page via PRG instead.
pub async fn reactions_fragment(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<super::pages::FragmentQuery>,
) -> Result<Response, ApiError> {
    let viewer = user.current.account.id;
    if announcement::find_published(&state.pool, id)
        .await?
        .is_none()
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let groups = announcement::reactions_for(&state.pool, &[id], Some(viewer))
        .await?
        .remove(&id)
        .unwrap_or_default();
    let reactions =
        crate::entities::announcement_reactions_json(&state.pool, &state.config.domain, &groups)
            .await?;
    let reactions = reactions.as_array().cloned().unwrap_or_default();
    let return_to = safe_return(query.return_to.as_deref(), "/announcements");
    Ok(view::reaction_chips_row(
        &format!("/web/announcements/{id}"),
        &reactions,
        Some(&user.csrf),
        &return_to,
        user.locale,
    )
    .into_response())
}

/// `POST /web/announcements/{id}/unreact/{name}`.
pub async fn unreact_action(
    State(state): State<AppState>,
    user: WebUser,
    Path((id, name)): Path<(i64, String)>,
    body: Bytes,
) -> Response {
    let account_id = user.current.account.id;
    run_verb(
        &user,
        &body,
        api::remove_reaction(&state, account_id, id, &name),
    )
    .await
}

/// `POST /web/announcements/{id}/dismiss` — mark read.
pub async fn dismiss_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let account_id = user.current.account.id;
    run_verb(&user, &body, api::mark_read(&state, account_id, id)).await
}
