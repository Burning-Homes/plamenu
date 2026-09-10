//! Identity-proof settings and the public profile's proof list.

use super::{
    i18n::Locale,
    session::{WebUser, csrf_rejection},
    settings::{
        SettingsQuery, bad_form, error_flash, field, form_pairs, redirect_to, saved_flash,
        settings_shell,
    },
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Query, State},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde_json::Value;

pub fn profile_proofs(proofs: &[Value], account_id: i64, locale: Locale) -> Markup {
    html! {
        @if !proofs.is_empty() {
            details.profile-identity-proofs {
                summary { (locale.text("identity-title")) }
                p { (locale.text("identity-meaning")) }
                ul { @for proof in proofs { li { code { (proof["subject"].as_str().unwrap_or_default()) } } } }
                a href=(format!("/api/v1/accounts/{account_id}/identity_statements")) { (locale.text("identity-view")) }
            }
        }
    }
}

pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<SettingsQuery>,
) -> Result<Response, ApiError> {
    let proofs = crate::identity::list(&state, &user.current.account).await?;
    let uri = crate::identity::actor_id(&state.config.domain, &user.current.account);
    let locale = user.locale;
    let body = html! {
        (saved_flash(query.saved.is_some(), &locale.text("identity-saved")))
        (error_flash(query.error.as_deref()))
        p { (locale.text("identity-purpose")) }
        p { (locale.text("identity-help")) }
        ol {
            li { (locale.text("identity-step-sign")) }
            li { (locale.text("identity-step-publish")) }
            li { (locale.text("identity-step-other-account")) }
        }
        p { (locale.text("identity-actor")) " " code { (uri) } }
        form.settings-form method="post" action="/web/settings/identity-proofs" {
            input type="hidden" name="csrf" value=(user.csrf);
            label.settings-field {
                span.settings-field__label { (locale.text("identity-document")) }
                textarea name="document" rows="12" maxlength="16384" required spellcheck="false" {}
            }
            button type="submit" { (locale.text("identity-publish")) }
        }
        @for proof in &proofs {
            form.settings-form method="post" action="/web/settings/identity-proofs/delete" {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="subject" value=(proof["subject"].as_str().unwrap_or_default());
                code { (proof["subject"].as_str().unwrap_or_default()) }
                button type="submit" { (locale.text("identity-remove")) }
            }
        }
        (profile_proofs(&proofs, user.current.account.id, locale))
    };
    Ok(settings_shell(
        &user,
        "/settings/identity-proofs",
        &locale.text("identity-title"),
        &body,
    )
    .into_response())
}

pub async fn publish(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    action(state, user, body, false).await
}

pub async fn remove(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    action(state, user, body, true).await
}

async fn action(state: AppState, user: WebUser, body: Bytes, remove: bool) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(e) => return bad_form(e),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    let result = if remove {
        crate::identity::remove(
            &state,
            user.current.account.id,
            field(&pairs, "subject").unwrap_or_default(),
        )
        .await
    } else {
        match serde_json::from_str(field(&pairs, "document").unwrap_or_default()) {
            Ok(document) => {
                crate::identity::publish(&state, user.current.account.id, document).await
            }
            Err(_) => Err(ApiError::Unprocessable(
                user.locale.plain("identity-invalid-json"),
            )),
        }
    };
    match result {
        Ok(_) => redirect_to("/settings/identity-proofs?saved=1"),
        Err(ApiError::Unprocessable(message)) => {
            let query = serde_urlencoded::to_string([("error", message)]).unwrap_or_default();
            redirect_to(&format!("/settings/identity-proofs?{query}"))
        }
        Err(e) => e.into_response(),
    }
}
