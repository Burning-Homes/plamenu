use super::{
    AppState, Form, Markup, Response, State, WebAdmin, back, csrf_rejection, error, html,
    permission,
};
use crate::discovery::seeds;
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

pub(super) fn controls(admin: &WebAdmin) -> Markup {
    let l = admin.user.locale;
    html! {
        details.discovery-details.discovery-transfers {
            summary {(l.text("discovery-transfer-title"))}
            p.settings-field__hint {(l.text("discovery-transfer-hint"))}
            form method="post" action="/web/admin/discovery/bootstrap" {
                input type="hidden" name="csrf" value=(admin.user.csrf);
                p {(l.text("discovery-bundled-details"))}
                button type="submit" {(l.text("discovery-bootstrap"))}
            }
            form method="post" action="/web/admin/discovery/import" {
                input type="hidden" name="csrf" value=(admin.user.csrf);
                label for="discovery-manifest" {(l.text("discovery-import-label"))}
                textarea id="discovery-manifest" name="manifest" rows="4" maxlength="65536" required
                    placeholder=r#"{"version":1,"sources":[{"origin":"https://example.org"}]}"# {};
                label.admin-check {
                    input type="checkbox" name="replace_bundled" value="1";
                    span {(l.text("discovery-replace-bundled"))}
                }
                button type="submit" {(l.text("discovery-import"))}
            }
            p {a href="/admin/discovery/export" {(l.text("discovery-export"))}}
        }
    }
}
#[derive(Deserialize)]
pub struct ImportForm {
    csrf: String,
    manifest: String,
    replace_bundled: Option<String>,
}
#[derive(Deserialize)]
pub struct BootstrapForm {
    csrf: String,
}

pub async fn import(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(f): Form<ImportForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&f.csrf) {
        return Err(csrf_rejection());
    }
    if seeds::parse(&f.manifest).is_err() {
        return Ok(back("invalid"));
    }
    let result = seeds::import(&state.pool, &f.manifest, false, f.replace_bundled.is_some())
        .await
        .map_err(error)?;
    Ok(result_redirect(&result))
}
pub async fn bootstrap(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(f): Form<BootstrapForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    if !admin.user.csrf_ok(&f.csrf) {
        return Err(csrf_rejection());
    }
    let result = seeds::import(&state.pool, seeds::BUNDLED, true, false)
        .await
        .map_err(error)?;
    Ok(result_redirect(&result))
}
fn result_redirect(result: &plamenu_db::discovery_store::ImportResult) -> Response {
    Redirect::to(&format!(
        "/admin/discovery?notice=imported&admitted={}&existing={}&refused={}",
        result.admitted, result.existing, result.refused
    ))
    .into_response()
}
pub async fn export(State(state): State<AppState>, admin: WebAdmin) -> Result<Response, Response> {
    admin.require(permission::MANAGE_FEDERATION)?;
    let manifest = seeds::export(&state.pool).await.map_err(error)?;
    Ok((
        [
            ("Content-Type", "application/json"),
            (
                "Content-Disposition",
                "attachment; filename=discovery-sources.json",
            ),
            ("Cache-Control", "private, no-store"),
        ],
        manifest,
    )
        .into_response())
}
