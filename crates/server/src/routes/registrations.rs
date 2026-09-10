//! `POST /api/v1/accounts` — self-service registration
//! (`Api::V1::AccountsController#create` + `AppSignUpService`).
//!
//! Authenticated with an app-level (`client_credentials`) token carrying
//! `write:accounts`; a successful sign-up answers with a user-level access
//! token for the new (still unconfirmed) user, which can drive the
//! `/api/v1/emails/*` endpoints until the address is confirmed.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::parse_body;
use crate::AppState;
use crate::auth::{AppToken, generate_secret, hash_secret};
use crate::error::ApiError;
use crate::instance_policy::RemoteIp;
use crate::registration::{self, SignUpParams};

#[derive(Deserialize)]
struct SignUpBody {
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    /// Booleans arrive as JSON `true` or form `"true"`/`"1"`.
    agreement: Option<Value>,
    locale: Option<String>,
    reason: Option<String>,
    invite_code: Option<String>,
    /// IANA time zone; normalised against the known inventory, an
    /// unknown value is dropped (matches Rails' lenient assignment).
    time_zone: Option<String>,
    /// Date of birth (`YYYY-MM-DD`), validated only when the server configures a
    /// minimum age; otherwise ignored and never stored.
    date_of_birth: Option<String>,
}

fn truthy_value(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "true" | "1" | "yes" | "on"),
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

/// `POST /api/v1/accounts`.
pub async fn create(
    State(state): State<AppState>,
    token: AppToken,
    RemoteIp(remote_ip): RemoteIp,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // Mastodon: `doorkeeper_authorize! :write, :'write:accounts'`.
    token.require_scope("write:accounts")?;

    let input: SignUpBody = parse_body(&headers, &body)?;
    let user = registration::sign_up(
        &state,
        token.app.id,
        remote_ip,
        SignUpParams {
            username: input.username.as_deref().unwrap_or("").trim(),
            email: input.email.as_deref(),
            password: input.password.as_deref().unwrap_or(""),
            agreement: truthy_value(input.agreement.as_ref()),
            locale: input.locale.as_deref().filter(|l| !l.is_empty()),
            reason: input.reason.as_deref(),
            invite_code: input.invite_code.as_deref(),
            time_zone: input
                .time_zone
                .as_deref()
                .and_then(crate::time_zones::normalize),
            date_of_birth: input.date_of_birth.as_deref(),
        },
    )
    .await?;

    // The sign-up token: the app's own scopes, bound to the new user —
    // Mastodon's `AppSignUpService#create_access_token!`.
    let access_token = generate_secret();
    let user_agent = crate::auth::user_agent_string(&headers);
    let ip = remote_ip.map(|ip| ip.to_string());
    let stored = plamenu_db::oauth::create_token_with_meta(
        &state.pool,
        &hash_secret(&access_token),
        token.app.id,
        Some(user.id),
        &token.app.scopes,
        plamenu_db::oauth::SessionMeta {
            user_agent: user_agent.as_deref(),
            ip: ip.as_deref(),
        },
    )
    .await?;
    Ok(Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "scope": stored.scopes,
        "created_at": stored.created_at.unix_timestamp(),
    })))
}

/// `GET /invite/{code}` — the shareable invite link (Mastodon's
/// `InvitesController#show`). Browsers are sent to the sign-up form with the
/// code attached; a JSON request gets the bootstrap apps consume. A
/// known-but-dead code is a 401 and an unknown one the usual 404.
pub async fn show_invite(
    State(state): State<AppState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse;

    let valid = plamenu_db::invite::find_valid_by_code(&state.pool, &code)
        .await?
        .is_some();
    if !valid {
        if plamenu_db::invite::find_by_code(&state.pool, &code)
            .await?
            .is_some()
        {
            return Err(ApiError::Unauthorized(
                "This invite is no longer valid".into(),
            ));
        }
        return Err(ApiError::NotFound);
    }
    let wants_json = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("json"));
    if wants_json {
        Ok(Json(json!({
            "invite_code": code,
            "instance_api_url": format!("https://{}/api/v2/instance", state.config.domain),
        }))
        .into_response())
    } else {
        let target = format!("/signup?invite_code={code}");
        Ok(axum::response::Redirect::to(&target).into_response())
    }
}

/// The v1/v2 instance entities' registration block inputs.
pub async fn registrations_json(state: &AppState) -> Result<Value, ApiError> {
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    let mode = settings.registrations_mode();
    let enabled = registration::open_for_registrations(state).await?;
    Ok(json!({
        "enabled": enabled,
        "approval_required": mode == plamenu_db::instance_settings::RegistrationsMode::Approved,
        "message": Value::Null,
    }))
}
