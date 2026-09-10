//! Strike-appeal review (Mastodon's `Admin::Disputes::AppealsController`).
//! The queue shows pending appeals with their strike context; approving
//! reverses the strike through `crate::moderation::approve_appeal`. All pages
//! require `MANAGE_APPEALS`; decisions are audit-logged like Mastodon's.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::{account, appeal};
use serde::Deserialize;

use super::super::clock::ViewerClock;
use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
    status: Option<String>,
}

/// `GET /admin/appeals` — pending appeals by default, `?status=resolved` for
/// the recent history.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_APPEALS)?;

    let pending_only = query.status.as_deref() != Some("resolved");
    let appeals = appeal::list_admin(&state.pool, pending_only, PAGE_LIMIT)
        .await
        .map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(
            query.flash.as_deref(),
            "That appeal could not be resolved (it may already be decided).",
        ))
        nav.admin-filter {
            @if pending_only {
                strong { "Pending" } " · " a href="/admin/appeals?status=resolved" { "Resolved" }
            } @else {
                a href="/admin/appeals" { "Pending" } " · " strong { "Resolved" }
            }
        }
        section.admin-list {
            @if appeals.is_empty() {
                p.empty {
                    @if pending_only { "No appeals are waiting for review." }
                    @else { "No appeals have been resolved." }
                }
            }
            @for item in &appeals {
                (appeal_row(item, csrf, pending_only, admin.clock()))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/appeals", "Appeals", &body).into_response())
}

fn appeal_row(
    item: &appeal::AdminAppeal,
    csrf: &str,
    pending: bool,
    clock: &ViewerClock,
) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong {
                    "@" (item.username) " appeals a " (item.strike_action) " strike"
                }
                span.admin-table__sub { (clock.element_date(item.appeal.created_at)) }
            }
            @if !item.strike_text.is_empty() {
                p.admin-table__sub { "Strike reason: " (item.strike_text) }
            }
            p.admin-note__body { (item.appeal.text) }
            @if pending {
                div.admin-actions {
                    form method="post" action=(format!("/web/admin/appeals/{}/approve", item.appeal.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Approve (reverse the action)" }
                    }
                    form method="post" action=(format!("/web/admin/appeals/{}/reject", item.appeal.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        button.admin-danger type="submit" { "Reject" }
                    }
                }
            } @else {
                p.admin-table__sub {
                    @if item.appeal.approved_at.is_some() { "Approved" } @else { "Rejected" }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/appeals/{id}/approve` — reverse the strike and resolve.
pub async fn approve(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    decide(state, admin, id, &form.csrf, true).await
}

/// `POST /web/admin/appeals/{id}/reject` — let the strike stand.
pub async fn reject(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    decide(state, admin, id, &form.csrf, false).await
}

async fn decide(
    state: AppState,
    admin: WebAdmin,
    id: i64,
    csrf: &str,
    approve: bool,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_APPEALS)?;
    if !admin.user.csrf_ok(csrf) {
        return Err(csrf_rejection());
    }
    let Some(target) = appeal::find_by_id(&state.pool, id).await.map_err(api_err)? else {
        return Ok(redirect_appeals("error"));
    };
    let moderator = admin.user.current.account.id;
    let resolved = if approve {
        crate::moderation::approve_appeal(&state, &target, moderator)
            .await
            .map_err(IntoResponse::into_response)?
    } else {
        crate::moderation::reject_appeal(&state, &target, moderator)
            .await
            .map_err(IntoResponse::into_response)?
    };
    if !resolved {
        return Ok(redirect_appeals("error"));
    }
    if let Some(appellant) = account::find_by_id(&state.pool, target.account_id)
        .await
        .map_err(api_err)?
    {
        admin_log::record(
            &state.pool,
            moderator,
            if approve { "approve" } else { "reject" },
            &admin_log::Target::appeal(target.id, &appellant),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_appeals("applied"))
}

fn redirect_appeals(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/appeals?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
