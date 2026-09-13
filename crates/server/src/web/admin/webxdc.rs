//! Server-wide Webxdc storage and lifecycle controls.
use axum::extract::{Form, Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use maud::{Markup, html};
use plamenu_db::{account, role::permission, webxdc};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::webxdc::storage_size as size;
use crate::{AppState, admin_log, error::ApiError, web::session::csrf_rejection};

#[derive(Default, Deserialize)]
pub struct IndexQuery {
    #[serde(default)]
    search: String,
    #[serde(default)]
    kind: String,
    max_id: Option<i64>,
    flash: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let usage = webxdc::storage_usage(&state.pool, None)
        .await
        .map_err(api_err)?;
    let limits = webxdc::limits(&state.pool).await.map_err(api_err)?;
    let rows = webxdc::admin_sessions(&state.pool, &query.search, &query.kind, query.max_id, 40)
        .await
        .map_err(api_err)?;
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The session action could not be completed."))
        p.admin__lead { (size(usage.total_bytes())) " of " (limits.total_mb) " MiB used · " (usage.sessions) " sessions · " (usage.packages) " apps" }
        p { "Apps: " (size(usage.package_bytes)) " · Session data: " (size(usage.data_bytes)) }
        @if admin.role.can(permission::MANAGE_SETTINGS) {
            p { a href="/admin/settings?open=webxdc" { "Configure Webxdc quotas and upload limits →" } }
        }
        details { summary { "How storage is counted" }
            p { "Each identical app is stored once, including its ZIP and expanded files. Durable updates belong to individual sessions. Totals measure retained payload bytes; database overhead and storage in users’ browsers are excluded. Ended sessions keep their data until deleted." }
        }
        form.admin-filter method="get" action="/admin/webxdc" {
            label { "Session or creator" input name="search" value=(&query.search); }
            label { "Show" select name="kind" {
                @for (value,label) in [("","All sessions"),("local","Hosted here"),("remote","Remote caches"),("ended","Ended")] {
                    option value=(value) selected[query.kind == value] { (label) }
                }
            } }
            button type="submit" { "Search" }
        }
        (crate::web::view::data_table(&html! {
            thead { tr { th { "Session" } th { "Status" } th { "App storage" } th { "Session data" } } }
            tbody {
                @if rows.is_empty() { tr { td colspan="4" { "No sessions match." } } }
                @for row in &rows {
                    tr {
                        td { a href=(format!("/admin/webxdc/{}",row.id)) { (&row.name) }
                            span.admin-table__sub title=(&row.creator_uri) { (&row.creator_label) } }
                        td { @if row.local { "Hosted here" } @else { "Remote cache" }
                            span.admin-table__sub { @if row.ended_at.is_some() { "Ended" } @else { "Active" } } }
                        td { (size(row.package_bytes)) span.admin-table__sub { @if row.package_sessions == 1 { "One session" } @else { "Shared by " (row.package_sessions) " sessions" } } }
                        td { (size(row.data_bytes)) }
                    }
                }
            }
        }))
        @if rows.len() == 40 {
            @if let Some(last) = rows.last() {
                p.admin-pager { a href=(format!("/admin/webxdc?max_id={}&search={}&kind={}", last.id,
                    url::form_urlencoded::byte_serialize(query.search.as_bytes()).collect::<String>(), url::form_urlencoded::byte_serialize(query.kind.as_bytes()).collect::<String>())) { "Older →" } }
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/webxdc", "Webxdc sessions", &body).into_response())
}

pub async fn show(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    let session = webxdc::find(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let storage = webxdc::session_storage(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let creator = match session.creator_account_id {
        Some(id) => account::find_by_id(&state.pool, id)
            .await
            .map_err(api_err)?,
        None => None,
    };
    let local = coordinator.is_local();
    let reclaim = storage.data_bytes
        + if storage.package_sessions == 1 {
            storage.package_bytes
        } else {
            0
        };
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "The session action could not be completed."))
        p { a href="/admin/webxdc" { "← All sessions" } }
        p { @if local { "Hosted here" } @else { "Remote cache" } " · " @if session.ended() { "Ended" } @else { "Active" } }
        dl.admin-detail__grid {
            dt { "Creator" } dd { @if let Some(creator) = &creator { a href=(format!("/admin/accounts/{}",creator.id)) { "@" (&creator.username) } } @else { (&session.creator_uri) } }
            dt { "Created" } dd { (admin.clock().element_date(session.published_at)) }
            dt { "App" } dd { (&session.bundle_name) }
            dt { "App storage" } dd { (size(storage.package_bytes)) @if storage.package_sessions == 1 { " · One session" } @else { " · Shared by " (storage.package_sessions) " sessions" } }
            dt { "Session data" } dd { (size(storage.data_bytes)) " · " (session.last_serial) " durable updates" }
            dt { "Participants" } dd { (storage.members) " account" @if storage.members != 1 { "s" } " · " (storage.guests) " guest" @if storage.guests != 1 { "s" } }
            dt { "Storage released by deletion" } dd { (size(reclaim)) }
        }
        p { a href=(format!("/webxdc/session/{id}")) { "View session →" } }
        @if local && !session.ended() {
            p { "Ending stops the app for everyone. Data is retained for 30 days unless you delete it sooner." }
            (action_form(&admin,id,"close","End session"))
        }
        details { summary { @if local { "Delete session" } @else { "Remove remote cache" } }
            p { @if local {
                "Permanently delete this session and its data for everyone, including federated participants."
            } @else {
                "Remove this cached session and leave it for all participants on this server. The original session stays on its host and can be opened again later."
            } " Shared app files are removed only when no other session uses them." }
            (action_form(&admin,id,if local {"delete"} else {"evict"},if local {"Delete session and data"} else {"Remove cache and leave"}))
        }
    };
    Ok(admin_shell(&admin, "/admin/webxdc", &session.name, &body).into_response())
}

fn action_form(admin: &WebAdmin, id: i64, op: &str, label: &str) -> Markup {
    html! { form method="post" action=(format!("/web/admin/webxdc/{id}/op")) {
        input type="hidden" name="csrf" value=(&admin.user.csrf);
        input type="hidden" name="op" value=(op);
        button.admin-danger type="submit" { (label) }
    } }
}

#[derive(Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBXDC)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let session = webxdc::find(&state.pool, id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    let coordinator = account::find_by_id(&state.pool, session.account_id)
        .await
        .map_err(api_err)?
        .ok_or_else(|| ApiError::NotFound.into_response())?;
    match form.op.as_str() {
        "close" if coordinator.is_local() => {
            crate::webxdc::close_local(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        "delete" if coordinator.is_local() => {
            crate::webxdc::delete_local(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        "evict" if !coordinator.is_local() => {
            crate::webxdc::evict_remote(&state, &session)
                .await
                .map_err(IntoResponse::into_response)?;
        }
        _ => return Err(ApiError::BadRequest("Invalid session action".into()).into_response()),
    }
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        &form.op,
        &admin_log::Target::webxdc(&session),
    )
    .await
    .map_err(api_err)?;
    let target = if form.op == "close" {
        format!("/admin/webxdc/{id}?flash=applied")
    } else {
        "/admin/webxdc?flash=applied".to_owned()
    };
    Ok(Redirect::to(&target).into_response())
}

fn api_err(error: plamenu_db::DbError) -> Response {
    ApiError::from(error).into_response()
}
