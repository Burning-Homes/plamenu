//! Instance-rule management for the admin dashboard. These are the published
//! server policies exposed through `GET /api/v1/instance/rules`; reports keep
//! resolving soft-deleted rules by id, so deletes here call `db::rule::delete`.
//! All pages require `MANAGE_RULES`.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::rule::{self, Rule};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

const RULE_TEXT_LIMIT: usize = 300;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/rules` — list live instance rules, with inline edit/delete
/// forms and a create form.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_RULES)?;

    let rules = rule::list_ordered(&state.pool).await.map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That rule could not be saved."))
        section.admin-form {
            h3 { "Add rule" }
            form method="post" action="/web/admin/rules" {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Text"
                    textarea name="text" rows="2" maxlength=(RULE_TEXT_LIMIT) required {}
                }
                label {
                    "Hint"
                    textarea name="hint" rows="2" {}
                }
                button type="submit" { "Add rule" }
            }
        }
        section.admin-list {
            h3 { "Current rules" }
            @if rules.is_empty() {
                p.empty { "No rules have been published." }
            }
            @for item in &rules {
                (rule_row(item, csrf))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/rules", "Rules", &body).into_response())
}

fn rule_row(item: &Rule, csrf: &str) -> Markup {
    html! {
        article.admin-record {
            form.admin-form method="post" action=(format!("/web/admin/rules/{}/update", item.id)) {
                input type="hidden" name="csrf" value=(csrf);
                div.admin-record__head {
                    strong { "Rule #" (item.id) }
                    span.admin-table__sub { "Priority " (item.priority) }
                }
                label {
                    "Text"
                    textarea name="text" rows="2" maxlength=(RULE_TEXT_LIMIT) required { (item.text) }
                }
                label {
                    "Hint"
                    textarea name="hint" rows="2" { (item.hint) }
                }
                label {
                    "Priority"
                    input type="number" name="priority" value=(item.priority);
                }
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
            form method="post" action=(format!("/web/admin/rules/{}/delete", item.id)) {
                input type="hidden" name="csrf" value=(csrf);
                button.admin-danger type="submit" { "Delete" }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RuleForm {
    csrf: String,
    text: String,
    #[serde(default)]
    hint: String,
    priority: Option<i32>,
}

/// `POST /web/admin/rules` — add a rule after the current live list.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<RuleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_RULES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(text) = valid_rule_text(&form.text) else {
        return Ok(redirect_rules("error"));
    };
    let rule = rule::create(&state.pool, text, form.hint.trim(), None)
        .await
        .map_err(api_err)?;
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        "create",
        &admin_log::Target::rule(rule.id, &rule.text),
    )
    .await
    .map_err(api_err)?;
    Ok(redirect_rules("applied"))
}

/// `POST /web/admin/rules/{id}/update` — edit text, hint and display order.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<RuleForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_RULES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(text) = valid_rule_text(&form.text) else {
        return Ok(redirect_rules("error"));
    };
    let result = rule::update(
        &state.pool,
        id,
        Some(text),
        Some(form.hint.trim()),
        form.priority,
    )
    .await
    .map_err(api_err)?;
    if let Some(rule) = &result {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "update",
            &admin_log::Target::rule(rule.id, &rule.text),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_rules(if result.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/rules/{id}/delete` — soft-delete the rule.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_RULES)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let target = rule::find_by_ids(&state.pool, &[id])
        .await
        .map_err(api_err)?
        .into_iter()
        .next();
    let deleted = rule::delete(&state.pool, id).await.map_err(api_err)?;
    if deleted && let Some(rule) = target {
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "destroy",
            &admin_log::Target::rule(rule.id, &rule.text),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_rules(if deleted { "applied" } else { "error" }))
}

fn valid_rule_text(raw: &str) -> Option<&str> {
    let text = raw.trim();
    (!text.is_empty() && text.chars().count() <= RULE_TEXT_LIMIT).then_some(text)
}

fn redirect_rules(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/rules?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
