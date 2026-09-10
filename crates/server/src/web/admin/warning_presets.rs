//! Warning-preset management (Mastodon's `Admin::WarningPresetsController`):
//! canned strike texts the account-action form offers in a dropdown. All
//! pages require `MANAGE_SETTINGS`; like Mastodon, preset changes are not
//! audit-logged.

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::warning_preset::{self, WarningPreset};
use serde::Deserialize;

use super::{WebAdmin, admin_shell};
use crate::AppState;
use crate::web::session::csrf_rejection;

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
}

/// `GET /admin/warning-presets` — list presets with inline edit/delete forms
/// and a create form.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;

    let presets = warning_preset::list(&state.pool).await.map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(query.flash.as_deref(), "That preset could not be saved."))
        p.admin-table__sub {
            "Presets pre-fill the text of a moderation action; the moderator "
            "picks one from the dropdown in the account \"Take action\" form."
        }
        section.admin-form {
            h3 { "Add preset" }
            form method="post" action="/web/admin/warning-presets" {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Title"
                    input type="text" name="title" placeholder="Spam" required;
                }
                label {
                    "Text"
                    textarea name="text" rows="3" required {}
                }
                button type="submit" { "Add preset" }
            }
        }
        section.admin-list {
            h3 { "Presets" }
            @if presets.is_empty() {
                p.empty { "No warning presets yet." }
            }
            @for preset in &presets {
                (preset_row(preset, csrf))
            }
        }
    };
    Ok(admin_shell(&admin, "/admin/warning-presets", "Warning presets", &body).into_response())
}

fn preset_row(preset: &WarningPreset, csrf: &str) -> Markup {
    html! {
        article.admin-record {
            form.admin-form method="post" action=(format!("/web/admin/warning-presets/{}/update", preset.id)) {
                input type="hidden" name="csrf" value=(csrf);
                label {
                    "Title"
                    input type="text" name="title" value=(preset.title) required;
                }
                label {
                    "Text"
                    textarea name="text" rows="3" required { (preset.text) }
                }
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
            form method="post" action=(format!("/web/admin/warning-presets/{}/delete", preset.id)) {
                input type="hidden" name="csrf" value=(csrf);
                button.admin-danger type="submit" { "Delete" }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PresetForm {
    csrf: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
}

/// `POST /web/admin/warning-presets` — add a preset.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<PresetForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let text = form.text.trim();
    if text.is_empty() {
        return Ok(redirect_presets("error"));
    }
    warning_preset::create(&state.pool, form.title.trim(), text)
        .await
        .map_err(api_err)?;
    Ok(redirect_presets("applied"))
}

/// `POST /web/admin/warning-presets/{id}/update` — rewrite title and text.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<PresetForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let text = form.text.trim();
    if text.is_empty() {
        return Ok(redirect_presets("error"));
    }
    let updated = warning_preset::update(&state.pool, id, form.title.trim(), text)
        .await
        .map_err(api_err)?;
    Ok(redirect_presets(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/warning-presets/{id}/delete` — remove the preset.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let deleted = warning_preset::delete(&state.pool, id)
        .await
        .map_err(api_err)?;
    Ok(redirect_presets(if deleted { "applied" } else { "error" }))
}

fn redirect_presets(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/admin/warning-presets?flash={flash}"),
        )],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}
