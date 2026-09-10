//! `/api/v1/emails/*` — confirmation resend + check
//! (Mastodon's e-mail confirmation endpoints). Both endpoints exist for
//! tokens of not-yet-functional users, so they authenticate with
//! [`AnyStateUser`] instead of the regular extractor.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use plamenu_db::email::OutgoingEmail;
use plamenu_db::{account, instance_settings, user};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::parse_body;
use crate::auth::{AnyStateUser, generate_secret, hash_secret};
use crate::error::ApiError;
use crate::{AppState, registration};

#[derive(Deserialize)]
struct ResendBody {
    email: Option<String>,
}

/// `POST /api/v1/emails/confirmations` — re-send the confirmation
/// instructions, optionally to a corrected address.
pub async fn resend_confirmation(
    State(state): State<AppState>,
    AnyStateUser(current): AnyStateUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    // Only the application the user signed up with may drive this.
    let creator_app = user::created_by_application_id(&state.pool, current.user.id).await?;
    if creator_app != Some(current.app_id) {
        return Err(ApiError::Forbidden(
            "This method is only available to the application the user originally signed-up with"
                .into(),
        ));
    }
    if current.user.confirmed() {
        return Err(ApiError::Forbidden(
            "This method is only available while the e-mail is awaiting confirmation".into(),
        ));
    }

    let input: ResendBody = if body.is_empty() {
        ResendBody { email: None }
    } else {
        parse_body(&headers, &body)?
    };
    let new_email = input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());

    // Render the mail *before* rotating the token, then rotate the token and
    // enqueue the mail in one transaction: a failed
    // enqueue rolls back the rotation, so the previous confirmation link stays
    // valid instead of being destroyed by an undelivered resend.
    let token = generate_secret();
    let settings = instance_settings::get(&state.pool).await?;
    let account = account::find_by_id(&state.pool, current.user.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let locale = crate::web::i18n::Locale::for_user(&state.pool, current.user.id).await?;
    let (subject, mail_body) = registration::render_confirmation_email(
        &state,
        &settings.site_title,
        &account,
        &token,
        locale,
    );
    let recipient = new_email.or(current.user.email.as_deref());
    let rotated = match recipient {
        Some(recipient) => {
            user::refresh_confirmation_token_with_mail(
                &state.pool,
                current.user.id,
                &hash_secret(&token),
                new_email,
                &OutgoingEmail {
                    recipient,
                    subject: &subject,
                    body: &mail_body,
                },
            )
            .await
        }
        // No deliverable address: rotate the token with nothing to enqueue,
        // matching the prior no-mail behaviour for an address-less resend.
        None => {
            user::refresh_confirmation_token(
                &state.pool,
                current.user.id,
                &hash_secret(&token),
                new_email,
            )
            .await
        }
    };
    rotated
        .map_err(|err| match err {
            plamenu_db::DbError::EmailTaken => ApiError::Unprocessable(
                "Validation failed: E-mail address has already been taken".into(),
            ),
            other => other.into(),
        })?
        .ok_or_else(|| {
            ApiError::Forbidden(
                "This method is only available while the e-mail is awaiting confirmation".into(),
            )
        })?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/accounts/{id}/email_subscriptions` — a wire-compatible 404.
/// Upstream feature-flags its notification-newsletter system off by default
/// and answers exactly like this; Plamenu deliberately has no newsletter
/// system (M21 architecture decision).
pub async fn email_subscriptions() -> Result<Json<Value>, ApiError> {
    Err(ApiError::NotFound)
}

/// `GET /api/v1/emails/check_confirmation` — a bare `true`/`false`.
pub async fn check_confirmation(
    State(state): State<AppState>,
    AnyStateUser(current): AnyStateUser,
) -> Result<Json<Value>, ApiError> {
    let _ = &state;
    current.require_scope("read:accounts")?;
    Ok(Json(Value::Bool(current.user.confirmed())))
}
