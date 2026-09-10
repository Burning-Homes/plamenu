//! Relay management for the admin dashboard — Mastodon's `admin/relays`.
//! Adding a relay subscribes immediately (its controller calls `enable!`
//! right after `save`); the state column tracks the handshake. All pages
//! require `MANAGE_FEDERATION` (Mastodon's `RelayPolicy`).

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::relay::{self, Relay};
use plamenu_db::role::permission;
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/relays` — list relays with their handshake state, plus the
/// subscribe form.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;

    let relays = relay::list(&state.pool).await.map_err(api_err)?;
    let activity: std::collections::HashMap<i64, relay::RelayActivity> =
        relay::activity_totals(&state.pool)
            .await
            .map_err(api_err)?
            .into_iter()
            .collect();
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (flash_banner(query.flash.as_deref()))
        p.admin__lead {
            "Relays forward public posts between servers. Subscribing sends a "
            "follow request from this server's instance actor; traffic starts "
            "once the relay accepts."
        }
        section.admin-form {
            h3 { "Add a relay" }
            form method="post" action="/web/admin/relays" {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Relay inbox URL, or an instance address"
                    input type="text" name="inbox_url" placeholder="relay.example or mobilizon.local"
                        required;
                }
                p.admin__hint {
                    "A bare address (\"mobilizon.local\") is resolved to that "
                    "server's own relay actor. For Mobilizon that is the only "
                    "channel carrying events organized by a person rather than "
                    "by a group — a Mobilizon profile cannot be followed."
                }
                button type="submit" { "Save and subscribe" }
            }
        }
        section.admin-list {
            h3 { "Relays" }
            @if relays.is_empty() {
                p.empty { "No relays configured." }
            }
            @for item in &relays {
                (relay_row(item, activity.get(&item.id).copied().unwrap_or_default(), csrf))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/relays", "Relays", &body).into_response())
}

fn relay_row(item: &Relay, activity: relay::RelayActivity, csrf: &str) -> Markup {
    let state_label = match item.state.as_str() {
        "accepted" => "Enabled",
        "pending" => "Waiting for approval",
        "rejected" => "Rejected",
        _ => "Disabled",
    };
    html! {
        article.admin-record {
            div.admin-record__head {
                strong { (item.inbox_url) }
                span.admin-badge { (state_label) }
            }
            p.admin-record__stats {
                (activity.total) " activities received — " (activity.last_week) " in the last 7 days"
            }
            div.admin-actions {
                @if item.state == "accepted" || item.state == "pending" {
                    form method="post" action=(format!("/web/admin/relays/{}/op", item.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="op" value="disable";
                        button type="submit" { "Disable" }
                    }
                } @else {
                    form method="post" action=(format!("/web/admin/relays/{}/op", item.id)) {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="op" value="enable";
                        button type="submit" { "Enable" }
                    }
                }
                form method="post" action=(format!("/web/admin/relays/{}/delete", item.id)) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Delete" }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateForm {
    csrf: String,
    inbox_url: String,
}

/// `POST /web/admin/relays` — register and immediately subscribe.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<CreateForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    // An inbox URL is taken as given; a bare instance address is resolved to its
    // relay actor's inbox (Mobilizon's lives at `/relay`, and its inbox is a
    // different URL again — see `relays::resolve_relay_inbox`).
    let Ok((inbox_url, actor_uri)) =
        crate::relays::resolve_relay_inbox(&state, &form.inbox_url).await
    else {
        return Ok(redirect_relays("error"));
    };
    let Some(created) = relay::create(&state.pool, &inbox_url, actor_uri.as_deref())
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_relays("duplicate"));
    };
    crate::relays::enable(&state, created.id)
        .await
        .map_err(IntoResponse::into_response)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "create",
        &admin_log::Target::relay(created.id, &created.inbox_url),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_relays("applied"))
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/relays/{id}/op` — enable (re-subscribe) or disable.
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let done = match form.op.as_str() {
        "enable" => crate::relays::enable(&state, id)
            .await
            .map_err(IntoResponse::into_response)?,
        "disable" => crate::relays::disable(&state, id)
            .await
            .map_err(IntoResponse::into_response)?,
        _ => false,
    };
    if done && let Some(target) = relay::find(&state.pool, id).await.map_err(api_err)? {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            &form.op,
            &admin_log::Target::relay(target.id, &target.inbox_url),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_relays(if done { "applied" } else { "error" }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/relays/{id}/delete` — unsubscribe if needed and remove.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = relay::find(&state.pool, id).await.map_err(api_err)?;
    let done = crate::relays::remove(&state, id)
        .await
        .map_err(IntoResponse::into_response)?;
    if done && let Some(target) = target {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "destroy",
            &admin_log::Target::relay(target.id, &target.inbox_url),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_relays(if done { "applied" } else { "error" }))
}

/// Relay-specific flash codes, falling through to the shared banner.
fn flash_banner(flash: Option<&str>) -> Markup {
    match flash {
        Some("duplicate") => html! {
            p.admin-flash.is-error role="alert" { "That relay is already configured." }
        },
        _ => super::flash_banner(
            flash,
            "That relay could not be saved. The inbox must be an https:// URL.",
        ),
    }
}

fn redirect_relays(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/relays?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
