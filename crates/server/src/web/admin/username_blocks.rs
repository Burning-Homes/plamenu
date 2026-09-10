//! Username-reservation management (Mastodon's
//! `Admin::UsernameBlocksController`): the sign-up blocklist enforced by
//! `crate::registration`. All pages require `MANAGE_BLOCKS`; create/update/
//! destroy are audit-logged like Mastodon's.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::username_block::{self, UsernameBlock};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/username-blocks` — list reservations with inline edit/delete
/// forms and a create form.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;

    let blocks = username_block::list(&state.pool).await.map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That reservation could not be saved."))
        p.admin-table__sub {
            "Reserved names are refused at sign-up. Matching ignores case and "
            "common digit stand-ins (\"4dm1n\" hits \"admin\"); a contains "
            "rule also blocks any username the text appears in. \"Allow with "
            "approval\" sends the sign-up to the approval queue instead of "
            "refusing it."
        }
        section.admin-form {
            h3 { "Reserve a username" }
            form method="post" action="/web/admin/username-blocks" {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Username"
                    input type="text" name="username" required;
                }
                (block_options(None))
                button type="submit" { "Reserve" }
            }
        }
        section.admin-list {
            h3 { "Reserved usernames" }
            @if blocks.is_empty() {
                p.empty { "No usernames are reserved." }
            }
            @for block in &blocks {
                (block_row(block, csrf))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/username-blocks", "Usernames", &body).into_response())
}

/// The comparison dropdown + approval checkbox, shared by the create form
/// (`None`) and each row's edit form.
fn block_options(block: Option<&UsernameBlock>) -> Markup {
    let exact = block.is_none_or(|b| b.exact);
    let allow = block.is_some_and(|b| b.allow_with_approval);
    html! {
        label {
            "Comparison"
            select name="comparison" {
                option value="equals" selected[exact] { "Equals" }
                option value="contains" selected[!exact] { "Contains" }
            }
        }
        label.admin-check {
            input type="checkbox" name="allow_with_approval" value="1" checked[allow];
            span { "Allow with approval" }
        }
    }
}

fn block_row(block: &UsernameBlock, csrf: &str) -> Markup {
    html! {
        article.admin-record {
            form.admin-form method="post" action=(format!("/web/admin/username-blocks/{}/update", block.id)) {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Username"
                    input type="text" name="username" value=(block.username) required;
                }
                (block_options(Some(block)))
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
            form method="post" action=(format!("/web/admin/username-blocks/{}/delete", block.id)) {
                input type="hidden" name="csrf" value=(csrf);
                button.admin-danger type="submit" { "Delete" }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BlockForm {
    csrf: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    comparison: String,
    #[serde(default)]
    allow_with_approval: Option<String>,
}

/// `POST /web/admin/username-blocks` — reserve a username.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<BlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let username = form.username.trim();
    if username.is_empty() {
        return Ok(redirect_blocks("error"));
    }
    let created = username_block::create(
        &state.pool,
        username,
        form.comparison != "contains",
        form.allow_with_approval.is_some(),
    )
    .await;
    let Ok(block) = created else {
        // Duplicate reservation (unique on lower(username)).
        return Ok(redirect_blocks("error"));
    };
    log_block(&state, &admin, "create", &block).await?;
    Ok(redirect_blocks("applied"))
}

/// `POST /web/admin/username-blocks/{id}/update` — rewrite a reservation.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<BlockForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let username = form.username.trim();
    if username.is_empty() {
        return Ok(redirect_blocks("error"));
    }
    let updated = username_block::update(
        &state.pool,
        id,
        username,
        form.comparison != "contains",
        form.allow_with_approval.is_some(),
    )
    .await;
    let Ok(Some(block)) = updated else {
        return Ok(redirect_blocks("error"));
    };
    log_block(&state, &admin, "update", &block).await?;
    Ok(redirect_blocks("applied"))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/username-blocks/{id}/delete` — lift the reservation.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_BLOCKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = username_block::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?;
    let deleted = username_block::delete(&state.pool, id)
        .await
        .map_err(api_err)?;
    if deleted && let Some(block) = target {
        log_block(&state, &admin, "destroy", &block).await?;
    }
    Ok(redirect_blocks(if deleted { "applied" } else { "error" }))
}

async fn log_block(
    state: &AppState,
    admin: &WebAdmin,
    action: &str,
    block: &UsernameBlock,
) -> Result<(), Response> {
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        action,
        &admin_log::Target::username_block(block.id, &block.username),
    )
    .await
    .map_err(api_err)
}

fn redirect_blocks(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/username-blocks?flash={flash}"),
        )],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
