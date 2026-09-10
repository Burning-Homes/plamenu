//! Terms of Service editor (Mastodon's `Admin::TermsOfService` pages): a
//! single Markdown draft rewritten in place until published, plus the history
//! of published versions. The current version is served publicly at
//! `/terms-of-service` and `GET /api/v1/instance/terms_of_service`. All pages
//! require `MANAGE_SETTINGS`; like Mastodon, only publishing is audit-logged.

use axum::extract::{Form, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::terms_of_service::{self, TermsOfService};
use serde::Deserialize;
use time::Date;
use time::macros::format_description;

use super::{WebAdmin, admin_shell};
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/terms-of-service` — the draft editor plus published history.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;

    let draft = terms_of_service::draft(&state.pool)
        .await
        .map_err(api_err)?;
    let current = terms_of_service::current(&state.pool)
        .await
        .map_err(api_err)?;
    let versions = terms_of_service::list_published(&state.pool)
        .await
        .map_err(api_err)?;
    // A fresh draft starts from the current published text, like Mastodon's.
    let seed_text = draft
        .as_ref()
        .map(|d| d.text.as_str())
        .or_else(|| current.as_ref().map(|c| c.text.as_str()))
        .unwrap_or("");
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(
            query.flash.as_deref(),
            "The terms could not be saved — text is required and the effective date must be a valid date.",
        ))
        section.admin-form {
            h3 { @if draft.is_some() { "Draft" } @else { "New draft" } }
            p.admin-table__sub {
                "Markdown. Saving keeps the draft private; publishing makes it "
                "the terms served at "
                a href="/terms-of-service" { "/terms-of-service" }
                " once the effective date arrives (immediately when left empty)."
            }
            form method="post" action="/web/admin/terms-of-service" {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Text"
                    textarea name="text" rows="16" required { (seed_text) }
                }
                label {
                    "Changelog (what changed, shown to users)"
                    textarea name="changelog" rows="3" {
                        (draft.as_ref().map_or("", |d| d.changelog.as_str()))
                    }
                }
                label {
                    "Effective date"
                    input type="date" name="effective_date"
                        value=(draft.as_ref().and_then(|d| d.effective_date).map(date_value).unwrap_or_default());
                }
                div.admin-actions {
                    button type="submit" name="op" value="save" { "Save draft" }
                    button type="submit" name="op" value="publish" { "Publish" }
                }
            }
        }
        section.admin-list {
            h3 { "Published versions" }
            @if versions.is_empty() {
                p.empty { "No terms of service have been published." }
            }
            @for version in &versions {
                (version_row(version, current.as_ref()))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/terms-of-service", "Terms of service", &body).into_response())
}

/// **Deliberately not zone-aware.** `effective_date` is a `DATE` column: a
/// calendar date the operator picked ("these terms take effect on the 1st"),
/// not an instant. Rendering it through a viewer's zone would shift it a day
/// for anyone east or west of UTC and change what the page claims. Leave it
/// as the plain ISO date the `<input type="date">` round-trips — see the G9
/// exemption in `TIMEZONE_HANDOFF.md`.
fn date_value(date: Date) -> String {
    date.format(format_description!("[year]-[month]-[day]"))
        .unwrap_or_default()
}

fn version_row(version: &TermsOfService, current: Option<&TermsOfService>) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                strong {
                    @match version.effective_date {
                        Some(date) => { "Effective " (date_value(date)) }
                        None => { "Effective on publication" }
                    }
                }
                @if current.is_some_and(|c| c.id == version.id) {
                    span.admin-table__sub { " (current)" }
                }
            }
            @if !version.changelog.is_empty() {
                p.admin-table__sub { (version.changelog) }
            }
            details {
                summary { "Show text" }
                pre.admin-tos__text { (version.text) }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TosForm {
    csrf: String,
    #[serde(default)]
    op: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    changelog: String,
    #[serde(default)]
    effective_date: String,
}

/// `POST /web/admin/terms-of-service` — save the draft; with `op=publish`,
/// publish it too.
pub async fn save(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<TosForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let text = form.text.trim();
    if text.is_empty() {
        return Ok(redirect_terms("error"));
    }
    let effective_date = match form.effective_date.trim() {
        "" => None,
        raw => match Date::parse(raw, format_description!("[year]-[month]-[day]")) {
            Ok(date) => Some(date),
            Err(_) => return Ok(redirect_terms("error")),
        },
    };
    let draft =
        terms_of_service::save_draft(&state.pool, text, form.changelog.trim(), effective_date)
            .await
            .map_err(api_err)?;
    if form.op == "publish" {
        let Some(published) = terms_of_service::publish(&state.pool, draft.id)
            .await
            .map_err(api_err)?
        else {
            return Ok(redirect_terms("error"));
        };
        admin_log::record(
            &state.pool,
            admin.user.current.account.id,
            "publish",
            &admin_log::Target::terms_of_service(
                published.id,
                published.effective_date.map(date_value).as_deref(),
            ),
        )
        .await
        .map_err(api_err)?;
    }
    Ok(redirect_terms("applied"))
}

fn redirect_terms(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/terms-of-service?flash={flash}"),
        )],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
