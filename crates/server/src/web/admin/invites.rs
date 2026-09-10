//! The site-wide invites overview for the admin dashboard (Mastodon's
//! `admin/invites`): every user's sign-up codes with their state, and a
//! deactivate verb that works on anyone's invite — unlike the per-user
//! `/settings/invites` page. Requires `MANAGE_INVITES`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::invite::{self, AdminInvite};
use plamenu_db::role::permission;
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::web::session::csrf_rejection;

/// Page size for the invite listing.
const PAGE_LIMIT: i64 = 50;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    max_id: Option<i64>,
    flash: Option<String>,
}

/// `GET /admin/invites` — every user's invites, newest first.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_INVITES)?;

    let invites = invite::list_all(&state.pool, query.max_id, PAGE_LIMIT)
        .await
        .map_err(api_err)?;
    let next_max = (i64::try_from(invites.len()).unwrap_or(i64::MAX) == PAGE_LIMIT)
        .then(|| invites.last().map(|row| row.invite.id))
        .flatten();

    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That invite could not be deactivated."))
        p.admin__lead {
            "Every sign-up invite on this server, across all users. "
            "Deactivating stops a code from being used for new registrations."
        }
        (crate::web::view::data_table(&html! {
            thead {
                tr {
                    th scope="col" { "Code" } th scope="col" { "Created by" } th scope="col" { "Uses" }
                    th scope="col" { "State" } th scope="col" { "" }
                }
            }
            tbody data-paged {
                @if invites.is_empty() {
                    tr { td colspan="5" { "No invites have been created." } }
                }
                @for row in &invites {
                    (invite_row(row, csrf))
                }
            }
        }))
        @if let Some(max) = next_max {
            p.admin-pager {
                a href=(format!("/admin/invites?max_id={max}")) { "Older →" }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/invites", "Invites", &body).into_response())
}

fn invite_row(row: &AdminInvite, csrf: &str) -> Markup {
    let invite = &row.invite;
    let uses = match invite.max_uses {
        Some(max) => format!("{} / {max}", invite.uses),
        None => format!("{} / ∞", invite.uses),
    };
    html! {
        tr {
            td { code { (invite.code) } }
            td { "@" (row.username) }
            td.admin-dimension__value { (uses) }
            td {
                @if invite.valid_for_use() {
                    span.admin-badge.is-active { "Active" }
                } @else {
                    span.admin-badge.is-disabled { "Expired" }
                }
            }
            td {
                @if invite.valid_for_use() {
                    form method="post" action=(format!("/web/admin/invites/{}/expire", invite.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        button.admin-danger type="submit" { "Deactivate" }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/invites/{id}/expire` — deactivate any user's invite.
pub async fn expire(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_INVITES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let expired = invite::expire_any(&state.pool, id).await.map_err(api_err)?;
    Ok(redirect_invites(if expired { "applied" } else { "error" }))
}

fn redirect_invites(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/invites?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
