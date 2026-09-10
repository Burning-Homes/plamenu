//! Webhook management for the admin dashboard. Mastodon exposes webhooks only
//! through the admin UI; this page lands the local registry and management
//! actions. Delivery workers can consume `db::webhook` rows later.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::extract::{Form, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use plamenu_db::role::permission;
use plamenu_db::webhook::{self, NewWebhook, Webhook, WebhookUpdate};
use serde::Deserialize;
use url::Url;

use super::{WebAdmin, admin_shell};
use crate::auth::generate_secret;
use crate::web::session::csrf_rejection;
use crate::{AppState, admin_log};

#[derive(Debug, Default, Deserialize)]
pub struct IndexQuery {
    flash: Option<String>,
    reveal: Option<i64>,
}

/// One-time webhook-secret reveals keyed to the admin who created/rotated the
/// hook. Secrets never enter a redirect URL, cookie, or persistent audit log.
static SECRET_REVEALS: LazyLock<tokio::sync::Mutex<HashMap<(i64, i64), String>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// `GET /admin/webhooks` — list webhooks and render create/edit controls.
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(query): Query<IndexQuery>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBHOOKS)?;

    let revealed = match query.reveal {
        Some(id) => SECRET_REVEALS
            .lock()
            .await
            .remove(&(admin.user.current.account.id, id))
            .map(|secret| (id, secret)),
        None => None,
    };
    render_index(&state, &admin, query.flash.as_deref(), revealed.as_ref()).await
}

async fn render_index(
    state: &AppState,
    admin: &WebAdmin,
    flash: Option<&str>,
    revealed: Option<&(i64, String)>,
) -> Result<Response, Response> {
    let webhooks = webhook::list(&state.pool).await.map_err(api_err)?;
    let csrf = admin.user.csrf.as_str();
    let body = html! {
        (super::flash_banner(flash, "That webhook change could not be saved."))
        @if let Some((id, secret)) = revealed {
            section.admin-form {
                h3 { "Copy this webhook secret now" }
                p { "Webhook #" (id) " will use this signing secret. It will not be shown again." }
                samp { (secret) }
            }
        }
        section.admin-form {
            h3 { "Create webhook" }
            p.admin-table__sub {
                "Webhook targets are operator-trusted infrastructure and may reach private networks. "
                "Do not place credentials in the URL; authenticate delivered bodies with the signing secret."
            }
            form method="post" action="/web/admin/webhooks" {
                input type="hidden" name="csrf" value=(csrf);
                (webhook_fields(None, admin))
                button type="submit" { "Create" }
            }
        }
        section.admin-list {
            h3 { "Webhooks" }
            @if webhooks.is_empty() {
                p.empty { "No webhooks configured." }
            }
            @for hook in &webhooks {
                (webhook_row(hook, csrf, admin))
            }
        }
    };
    Ok(admin_shell(admin, "/admin/webhooks", "Webhooks", &body).into_response())
}

fn webhook_row(hook: &Webhook, csrf: &str, admin: &WebAdmin) -> Markup {
    html! {
        article.admin-record {
            div.admin-record__head {
                div {
                    strong { (hook.url) }
                    span.admin-table__sub { "Secret fingerprint: " samp { (secret_fingerprint(&hook.secret)) } }
                }
                div.admin-actions {
                    @if hook.enabled {
                        span.admin-badge { "Enabled" }
                    } @else {
                        span.admin-badge.is-disabled { "Disabled" }
                    }
                }
            }
            div.admin-actions {
                @for event in &hook.events {
                    span.admin-badge { (webhook::event_label(event)) }
                }
            }
            form.admin-form method="post" action=(format!("/web/admin/webhooks/{}/update", hook.id)) {
                input type="hidden" name="csrf" value=(csrf);
                (webhook_fields(Some(hook), admin))
                div.admin-actions {
                    button type="submit" { "Save" }
                }
            }
            div.admin-actions {
                @if hook.enabled {
                    (op_form(hook.id, csrf, "disable", "Disable"))
                } @else {
                    (op_form(hook.id, csrf, "enable", "Enable"))
                }
                (op_form(hook.id, csrf, "rotate", "Rotate secret"))
                form method="post" action=(format!("/web/admin/webhooks/{}/delete", hook.id)) {
                    input type="hidden" name="csrf" value=(csrf);
                    button.admin-danger type="submit" { "Delete" }
                }
            }
        }
    }
}

fn webhook_fields(hook: Option<&Webhook>, admin: &WebAdmin) -> Markup {
    let url = hook.map_or("", |h| h.url.as_str());
    let template = hook.and_then(|h| h.template.as_deref()).unwrap_or("");
    html! {
        label {
            "Callback URL"
            input type="url" name="url" value=(url) placeholder="https://example.com/webhook" required;
        }
        fieldset.admin-form__group {
            legend { "Events" }
            div.admin-form__checks {
                @for event in webhook::EVENTS {
                    @let checked = hook.is_some_and(|h| h.events.iter().any(|selected| selected == *event));
                    @let allowed = webhook::permission_for_event(event).is_some_and(|p| admin.role.can(p));
                    label.admin-check {
                        input type="checkbox" name=(event_field(event)) value="1" checked[checked] disabled[!allowed];
                        span { (webhook::event_label(event)) }
                    }
                }
            }
        }
        label {
            "Template"
            textarea name="template" rows="4" placeholder="Leave empty to deliver the default JSON payload." { (template) }
        }
    }
}

fn op_form(id: i64, csrf: &str, op: &str, label: &str) -> Markup {
    html! {
        form method="post" action=(format!("/web/admin/webhooks/{id}/op")) {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name="op" value=(op);
            button type="submit" { (label) }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WebhookForm {
    csrf: String,
    url: String,
    account_approved: Option<String>,
    account_created: Option<String>,
    account_updated: Option<String>,
    report_created: Option<String>,
    report_updated: Option<String>,
    status_created: Option<String>,
    status_updated: Option<String>,
    #[serde(default)]
    template: String,
}

/// `POST /web/admin/webhooks` — create a webhook with a fresh shared secret.
pub async fn create(
    State(state): State<AppState>,
    admin: WebAdmin,
    Form(form): Form<WebhookForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBHOOKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(data) = parse_form(&admin, &form) else {
        return Ok(redirect_webhooks("error"));
    };
    let secret = generate_secret();
    let created = webhook::create(
        &state.pool,
        NewWebhook {
            url: &data.url,
            events: &data.events,
            secret: &secret,
            template: data.template.as_deref(),
        },
    )
    .await
    .map_err(api_err)?;
    log_webhook(&state, &admin, "create", created.id, &created.url).await?;
    remember_secret(&admin, created.id, secret).await;
    Ok(redirect_webhook_reveal(created.id))
}

/// `POST /web/admin/webhooks/{id}/update` — edit URL, events and template.
pub async fn update(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<WebhookForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBHOOKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(existing) = webhook::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_webhooks("error"));
    };
    if !can_manage_events(&admin, &existing.events) {
        return Ok(redirect_webhooks("error"));
    }
    let Some(data) = parse_form(&admin, &form) else {
        return Ok(redirect_webhooks("error"));
    };
    let updated = webhook::update(
        &state.pool,
        id,
        WebhookUpdate {
            url: &data.url,
            events: &data.events,
            template: data.template.as_deref(),
        },
    )
    .await
    .map_err(api_err)?;
    if let Some(hook) = &updated {
        log_webhook(&state, &admin, "update", hook.id, &hook.url).await?;
    }
    Ok(redirect_webhooks(if updated.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct OpForm {
    csrf: String,
    op: String,
}

/// `POST /web/admin/webhooks/{id}/op` — enable/disable or rotate the secret.
pub async fn op(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<OpForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBHOOKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let rotated_secret = (form.op == "rotate").then(generate_secret);
    let changed = match form.op.as_str() {
        "enable" => webhook::set_enabled(&state.pool, id, true).await,
        "disable" => webhook::set_enabled(&state.pool, id, false).await,
        "rotate" => {
            webhook::rotate_secret(
                &state.pool,
                id,
                rotated_secret.as_deref().expect("rotate secret exists"),
            )
            .await
        }
        _ => return Ok(redirect_webhooks("error")),
    }
    .map_err(api_err)?;
    if let Some(hook) = &changed {
        log_webhook(&state, &admin, &form.op, hook.id, &hook.url).await?;
    }
    if let (Some(hook), Some(secret)) = (&changed, rotated_secret) {
        remember_secret(&admin, hook.id, secret).await;
        return Ok(redirect_webhook_reveal(hook.id));
    }
    Ok(redirect_webhooks(if changed.is_some() {
        "applied"
    } else {
        "error"
    }))
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

/// `POST /web/admin/webhooks/{id}/delete` — destroy the webhook.
pub async fn delete(
    State(state): State<AppState>,
    admin: WebAdmin,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_WEBHOOKS)?;
    if !admin.user.csrf_ok(&form.csrf) {
        return Err(csrf_rejection());
    }
    let Some(existing) = webhook::find_by_id(&state.pool, id)
        .await
        .map_err(api_err)?
    else {
        return Ok(redirect_webhooks("error"));
    };
    if !can_manage_events(&admin, &existing.events) {
        return Ok(redirect_webhooks("error"));
    }
    let deleted = webhook::delete(&state.pool, id).await.map_err(api_err)?;
    if deleted {
        log_webhook(&state, &admin, "destroy", existing.id, &existing.url).await?;
    }
    Ok(redirect_webhooks(if deleted { "applied" } else { "error" }))
}

/// Appends a webhook verb to the audit log.
async fn log_webhook(
    state: &AppState,
    admin: &WebAdmin,
    verb: &str,
    id: i64,
    url: &str,
) -> Result<(), Response> {
    admin_log::record(
        &state.pool,
        admin.user.current.account.id,
        verb,
        &admin_log::Target::webhook(id, url),
    )
    .await
    .map_err(api_err)?;
    Ok(())
}

struct ParsedWebhookForm {
    url: String,
    events: Vec<String>,
    template: Option<String>,
}

fn parse_form(admin: &WebAdmin, form: &WebhookForm) -> Option<ParsedWebhookForm> {
    let url = clean_url(&form.url)?;
    let events = selected_events(form)?;
    if !can_manage_events(admin, &events) {
        return None;
    }
    Some(ParsedWebhookForm {
        url,
        events,
        template: blank_to_none(&form.template),
    })
}

fn selected_events(form: &WebhookForm) -> Option<Vec<String>> {
    let mut events = Vec::new();
    for (event, checked) in [
        (webhook::ACCOUNT_APPROVED, form.account_approved.is_some()),
        (webhook::ACCOUNT_CREATED, form.account_created.is_some()),
        (webhook::ACCOUNT_UPDATED, form.account_updated.is_some()),
        (webhook::REPORT_CREATED, form.report_created.is_some()),
        (webhook::REPORT_UPDATED, form.report_updated.is_some()),
        (webhook::STATUS_CREATED, form.status_created.is_some()),
        (webhook::STATUS_UPDATED, form.status_updated.is_some()),
    ] {
        if checked {
            events.push(event.to_owned());
        }
    }
    (!events.is_empty()).then_some(events)
}

fn can_manage_events(admin: &WebAdmin, events: &[String]) -> bool {
    webhook::required_permissions(events)
        .iter()
        .all(|permission| admin.role.can(*permission))
}

fn event_field(event: &str) -> &'static str {
    match event {
        webhook::ACCOUNT_APPROVED => "account_approved",
        webhook::ACCOUNT_CREATED => "account_created",
        webhook::ACCOUNT_UPDATED => "account_updated",
        webhook::REPORT_CREATED => "report_created",
        webhook::REPORT_UPDATED => "report_updated",
        webhook::STATUS_CREATED => "status_created",
        webhook::STATUS_UPDATED => "status_updated",
        _ => "unknown_event",
    }
}

fn clean_url(raw: &str) -> Option<String> {
    let mut parsed = Url::parse(raw.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    // Normalize the stored form (host case, default ports, percent encoding).
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

fn secret_fingerprint(secret: &str) -> String {
    let digest = crate::auth::hash_secret(secret);
    format!("sha256:{}", &digest[..12])
}

async fn remember_secret(admin: &WebAdmin, webhook_id: i64, secret: String) {
    let mut reveals = SECRET_REVEALS.lock().await;
    // Bound abandoned reveals. Ordinary use removes an entry on the first GET.
    if reveals.len() >= 256
        && let Some(oldest) = reveals.keys().next().copied()
    {
        reveals.remove(&oldest);
    }
    reveals.insert((admin.user.current.account.id, webhook_id), secret);
}

fn redirect_webhook_reveal(id: i64) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/webhooks?reveal={id}"))],
    )
        .into_response()
}

fn blank_to_none(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn redirect_webhooks(flash: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, format!("/admin/webhooks?flash={flash}"))],
    )
        .into_response()
}

fn api_err(err: plamenu_db::DbError) -> Response {
    crate::error::ApiError::from(err).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_urls_reject_embedded_credentials_queries_and_fragments() {
        for bad in [
            "https://user:secret@hooks.example/callback",
            "https://hooks.example/callback?token=secret",
            "https://hooks.example/callback#secret",
            "ftp://hooks.example/callback",
        ] {
            assert!(clean_url(bad).is_none(), "accepted {bad}");
        }
        assert_eq!(
            clean_url(" HTTPS://Hooks.Example:443/callback ").as_deref(),
            Some("https://hooks.example/callback")
        );
    }

    #[test]
    fn webhook_secret_fingerprint_discloses_only_a_suffix() {
        let secret = "unique-webhook-secret-sentinel";
        let fingerprint = secret_fingerprint(secret);
        assert!(fingerprint.starts_with("sha256:"));
        assert_eq!(fingerprint.len(), 19);
        assert!(!fingerprint.contains("secret"));
    }
}
