//! Native channel preferences augment the existing push and moderation settings.
use super::{i18n::Locale, layout, session::WebUser, settings};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::notification_delivery::{self, Preference};
pub(crate) fn label(l: Locale, kind: &str) -> String {
    if let Some((_, key, _)) = settings::PUSH_ALERTS.iter().find(|(k, _, _)| *k == kind) {
        return l.text(key);
    }
    l.text(match kind {
        "severed_relationships" => "notification-delivery-severed",
        "moderation_warning" => "notification-delivery-moderation",
        "annual_report" => "notification-delivery-annual",
        "admin.sign_up" => "notification-delivery-signup",
        "admin.report" => "notification-delivery-report",
        "added_to_collection" => "notification-delivery-collection-added",
        "collection_update" => "notification-delivery-collection-update",
        _ => "page-notifications",
    })
}
pub async fn page(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    user.current.require_scope("read:notifications")?;
    let preferences =
        notification_delivery::preferences(&state.pool, user.current.account.id).await?;
    let l = user.locale;
    let body = html! {section.column{
        h1{(l.text("notification-delivery-title"))}
        p{(l.text("notification-delivery-hint"))} p{(l.text("notification-delivery-digest-time")) " " (format!("{:02}:00",state.config.notifications.digest_hour))}
        @if !crate::mailer::enabled(&state){p.status-message{(l.text("notification-delivery-no-smtp"))}}
        p{a href="/settings/push"{(l.text("settings-push-title"))}" · " a href="/notifications/inbox"{(l.text("page-notifications"))}}
        form.settings-form method="post" action="/web/settings/notification-delivery" {
            input type="hidden" name="csrf" value=(user.csrf);
            @for p in preferences {fieldset {
                legend{(label(l,&p.kind))}
                @for (channel,key,checked) in [("in_app","notification-delivery-in-app",p.in_app),("email","notification-delivery-email",p.email),("email_digest","notification-delivery-digest",p.email_digest)] {
                    label {input type="checkbox" name=(format!("{channel}:{}",p.kind)) value="true" checked[checked]; " "(l.text(key))}
                }
            }}
            button type="submit"{(l.text("notification-delivery-save"))}
        }
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("notification-delivery-title"), Some(&user), &body),
    )
        .into_response())
}
pub async fn action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:notifications")?;
    let items = notification_delivery::TYPES
        .iter()
        .map(|kind| Preference {
            kind: (*kind).into(),
            in_app: settings::field(&form, &format!("in_app:{kind}")) == Some("true"),
            email: settings::field(&form, &format!("email:{kind}")) == Some("true"),
            email_digest: settings::field(&form, &format!("email_digest:{kind}")) == Some("true"),
        })
        .collect::<Vec<_>>();
    notification_delivery::update(&state.pool, user.current.account.id, &items).await?;
    Ok(settings::redirect_to("/settings/notification-delivery"))
}
