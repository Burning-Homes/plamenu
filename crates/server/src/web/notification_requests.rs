//! Filtered-notification requests — the web half of Mastodon's
//! `/api/v1/notifications/requests` surface (see
//! [`plamenu_db::notification_request`]). Notifications held by the viewer's
//! notification policy roll up into one request per sender; accepting a
//! request releases the held notifications and always shows that sender from
//! then on, dismissing discards them. The policy itself is tuned on the
//! Privacy and reach settings page.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use maud::html;
use plamenu_db::{account, notification_request};
use serde::Deserialize;
use serde_json::Value;

use super::session::{WebUser, csrf_rejection};
use super::settings::{bad_form, field, form_pairs, redirect_to, saved_flash};
use super::{layout, view};
use crate::entities::render_accounts;
use crate::error::ApiError;
use crate::state::AppState;

/// Page size, matching the API's default.
const LIMIT: i64 = 40;

#[derive(Deserialize)]
pub struct RequestsQuery {
    max_id: Option<i64>,
    saved: Option<String>,
}

/// `GET /notifications/requests` — one row per filtered sender, newest
/// activity first, with accept/dismiss on each.
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<RequestsQuery>,
) -> Response {
    let viewer_id = user.current.account.id;
    let requests =
        match notification_request::list(&state.pool, viewer_id, query.max_id, None, None, LIMIT)
            .await
        {
            Ok(requests) => requests,
            Err(err) => return ApiError::from(err).into_response(),
        };
    // One batched render for every sender on the page instead of a live
    // `account_json` per row; a torn row (sender since deleted) drops out of
    // `find_by_ids` and renders nothing rather than failing the page.
    let sender_ids: Vec<i64> = requests.iter().map(|r| r.from_account_id).collect();
    let senders = match account::find_by_ids(&state.pool, &sender_ids).await {
        Ok(senders) => senders,
        Err(err) => return ApiError::from(err).into_response(),
    };
    let entities =
        match render_accounts(&state.pool, &state.config.domain, &senders, Some(viewer_id)).await {
            Ok(entities) => entities,
            Err(err) => return err.into_response(),
        };
    let by_sender: HashMap<i64, Value> = senders
        .iter()
        .zip(entities)
        .map(|(sender, entity)| (sender.id, entity))
        .collect();
    let rows: Vec<_> = requests
        .iter()
        .filter_map(|request| {
            by_sender
                .get(&request.from_account_id)
                .map(|entity| (request, entity.clone()))
        })
        .collect();
    let full = requests.len() >= usize::try_from(LIMIT).unwrap_or(usize::MAX);
    let next = full
        .then(|| requests.last().map(|r| r.id))
        .flatten()
        .map(|id| format!("/notifications/requests?max_id={id}"));
    let clock = &user.clock;
    let locale = user.locale;
    let title = locale.text("notification-requests-title");
    let filtering_link = html! {
        a href="/settings/privacy" { (locale.text("notification-requests-hint-link")) }
    };
    let summary = |count: i64, updated_at| {
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", count);
        args.set("time", clock.stamp(updated_at));
        locale.text_with("notification-requests-summary", &args)
    };
    let body = html! {
        section.column {
            h1 { (view::icon("bell")) " " (title) }
            (saved_flash(query.saved.is_some(), &locale.text("notification-requests-saved")))
            p.settings-field__hint {
                (locale.markup("notification-requests-hint", &[("link", filtering_link)]))
            }
            @if rows.is_empty() {
                p.empty { (locale.text("notification-requests-empty")) }
            } @else {
                ul.relationships-list {
                    @for (request, entity) in &rows {
                        li.relationships-list__item {
                            (view::account_card(&view::Account(entity)))
                            span.settings-field__hint {
                                (summary(request.notifications_count, request.updated_at))
                            }
                            form method="post"
                                action=(format!("/web/notifications/requests/{}/accept", request.id)) {
                                input type="hidden" name="csrf" value=(user.csrf);
                                button type="submit" {
                                    (locale.text("notification-requests-accept"))
                                }
                            }
                            form method="post"
                                action=(format!("/web/notifications/requests/{}/dismiss", request.id)) {
                                input type="hidden" name="csrf" value=(user.csrf);
                                button.settings-button--danger type="submit" {
                                    (locale.text("notification-requests-dismiss"))
                                }
                            }
                        }
                    }
                }
            }
            @if let Some(href) = &next {
                nav.pager {
                    a.pager__more href=(href) { (locale.text("page-load-more")) }
                }
            }
        }
    };
    layout::shell(&title, Some(&user), &body).into_response()
}

/// Runs one accept/dismiss verb. A request already resolved elsewhere (double
/// submit, another tab) is treated as done rather than a 404 page.
async fn resolve(
    state: &AppState,
    user: &WebUser,
    request_id: i64,
    body: &Bytes,
    accept: bool,
) -> Response {
    let pairs = match form_pairs(body) {
        Ok(pairs) => pairs,
        Err(message) => return bad_form(message),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let account_id = user.current.account.id;
    let request = match notification_request::find(&state.pool, account_id, request_id).await {
        Ok(request) => request,
        Err(err) => return ApiError::from(err).into_response(),
    };
    if let Some(request) = request {
        let result = if accept {
            notification_request::accept(&state.pool, account_id, request.from_account_id).await
        } else {
            notification_request::dismiss(&state.pool, account_id, request.from_account_id).await
        };
        if let Err(err) = result {
            return ApiError::from(err).into_response();
        }
    }
    redirect_to("/notifications/requests?saved=1")
}

/// `POST /web/notifications/requests/{id}/accept`.
pub async fn accept_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(request_id): Path<i64>,
    body: Bytes,
) -> Response {
    resolve(&state, &user, request_id, &body, true).await
}

/// `POST /web/notifications/requests/{id}/dismiss`.
pub async fn dismiss_action(
    State(state): State<AppState>,
    user: WebUser,
    Path(request_id): Path<i64>,
    body: Bytes,
) -> Response {
    resolve(&state, &user, request_id, &body, false).await
}
