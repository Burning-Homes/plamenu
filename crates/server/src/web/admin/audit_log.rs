//! The audit-log page for the admin dashboard (Mastodon's
//! `admin/action_logs`) — the read surface over `admin_action_logs`, which
//! every admin mutation (web or REST) appends to. Requires `VIEW_AUDIT_LOG`;
//! the log is append-only, so this page is read-only.

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::admin_action_log::{self, AdminActionLog, LogFilter};
use plamenu_db::role::permission;
use serde::Deserialize;

use super::super::clock::{ViewerClock, zone_chip};
use super::{WebAdmin, admin_shell};
use crate::AppState;

/// Page size for the log listing.
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    // The moderator `<select>` submits `account_id=` (empty) for "Any"; treat
    // that as no filter rather than a parse error.
    #[serde(default, deserialize_with = "super::empty_as_none")]
    account_id: Option<i64>,
    action: Option<String>,
    target_type: Option<String>,
    #[serde(default, deserialize_with = "super::empty_as_none")]
    max_id: Option<i64>,
}

/// `GET /admin/audit-log` — the moderation audit trail, newest first,
/// filterable by moderator and target type.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::VIEW_AUDIT_LOG)?;

    let target_type = query
        .target_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let action = query
        .action
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned);
    let filter = LogFilter {
        account_id: query.account_id,
        action: action.clone(),
        target_type: target_type.clone(),
        max_id: query.max_id,
        limit: PAGE_LIMIT,
    };
    let logs = admin_action_log::list(&state.pool, &filter)
        .await
        .map_err(api_err)?;
    let actors = admin_action_log::actors(&state.pool)
        .await
        .map_err(api_err)?;
    let target_types = admin_action_log::target_types(&state.pool)
        .await
        .map_err(api_err)?;
    let actions = admin_action_log::actions(&state.pool)
        .await
        .map_err(api_err)?;
    let next_max = (i64::try_from(logs.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| logs.last().map(|log| log.id))
        .flatten();

    let body = html! {
        p.admin__lead {
            "Every moderation and server-administration action, newest first. "
            "Entries are recorded permanently and cannot be edited."
        }
        (zone_chip(admin.clock()))
        form.admin-filter method="get" action="/admin/audit-log" {
            label {
                "Moderator"
                select name="account_id" {
                    option value="" selected[query.account_id.is_none()] { "Any" }
                    @for actor in &actors {
                        option value=(actor.account_id)
                            selected[query.account_id == Some(actor.account_id)] {
                            "@" (actor.username)
                        }
                    }
                }
            }
            label {
                "Action"
                select name="action" {
                    option value="" selected[action.is_none()] { "Any" }
                    @for verb in &actions {
                        option value=(verb) selected[action.as_deref() == Some(verb)] {
                            (verb)
                        }
                    }
                }
            }
            label {
                "Target"
                select name="target_type" {
                    option value="" selected[target_type.is_none()] { "Any" }
                    @for kind in &target_types {
                        option value=(kind) selected[target_type.as_deref() == Some(kind)] {
                            (kind)
                        }
                    }
                }
            }
            button type="submit" { "Filter" }
        }
        section.admin-list data-paged {
            @if logs.is_empty() {
                p.empty { "No actions have been recorded." }
            }
            @for log in &logs {
                (log_row(log, admin.clock()))
            }
        }
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(older_href(max, query.account_id, action.as_deref(), target_type.as_deref())) {
                    "Older →"
                }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/audit-log", "Audit log", &body).into_response())
}

fn log_row(log: &AdminActionLog, clock: &ViewerClock) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong { "@" (log.account_username) }
                " " (log.action) " " (log.target_type) " "
                @if log.human_identifier.is_empty() {
                    "#" (log.target_id)
                } @else if let Some(permalink) = &log.permalink {
                    a href=(permalink) { (log.human_identifier) }
                } @else {
                    (log.human_identifier)
                }
            }
            span.admin-table__sub { (clock.element_absolute(log.created_at)) }
        }
    }
}

fn older_href(
    max_id: i64,
    account_id: Option<i64>,
    action: Option<&str>,
    target_type: Option<&str>,
) -> String {
    use std::fmt::Write;

    let mut href = format!("/admin/audit-log?max_id={max_id}");
    if let Some(account_id) = account_id {
        let _ = write!(href, "&account_id={account_id}");
    }
    if let Some(action) = action {
        let _ = write!(href, "&action={action}");
    }
    if let Some(target_type) = target_type {
        let _ = write!(href, "&target_type={target_type}");
    }
    href
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
