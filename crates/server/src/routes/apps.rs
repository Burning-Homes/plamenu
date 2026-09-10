//! `/api/v1/apps` — client (app) registration and token introspection.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use plamenu_db::oauth::{self, NewApp};
use serde::Deserialize;
use serde_json::Value;

use super::params::parse_body;
use crate::auth::{INVALID_TOKEN, generate_secret, hash_secret};
use crate::entities::application_json;
use crate::error::ApiError;
use crate::oauth_app;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct AppParams {
    pub client_name: String,
    /// A string (newline-separated, the classic form) or an array of strings
    /// (Mastodon 4.3+).
    pub redirect_uris: Value,
    pub scopes: Option<String>,
    pub website: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let params: AppParams = parse_body(&headers, &body)?;
    let redirect_uris: Vec<String> = match &params.redirect_uris {
        Value::String(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    // Share the strict `ApplicationExtension` validation the signed-in web
    // registration form already enforces, so this anonymous endpoint can't
    // store malformed names, dangerous redirect URIs, arbitrary website
    // schemes, or unknown scopes. Every failure is a 422 with a message,
    // matching Mastodon's registration error shape.
    let name = params.client_name.trim();
    oauth_app::validate_name(name).map_err(|invalid| unprocessable(&invalid))?;
    oauth_app::validate_redirect_uris(&redirect_uris).map_err(|invalid| unprocessable(&invalid))?;
    let website = oauth_app::validate_website(params.website.as_deref().unwrap_or_default())
        .map_err(|invalid| unprocessable(&invalid))?;
    let scopes = oauth_app::normalize_scopes(params.scopes.as_deref().unwrap_or_default())
        .map_err(|invalid| unprocessable(&invalid))?;

    let client_id = generate_secret();
    let client_secret = generate_secret();
    let app = oauth::create_app(
        &state.pool,
        NewApp {
            name,
            website: website.as_deref(),
            client_id: &client_id,
            client_secret_hash: &hash_secret(&client_secret),
            redirect_uris: &redirect_uris,
            scopes: &scopes,
        },
    )
    .await?;

    // The secret is shown exactly once; only its hash is stored.
    let mut entity = application_json(&app);
    entity["client_id"] = Value::String(client_id);
    entity["client_secret"] = Value::String(client_secret);
    Ok(Json(entity))
}

/// `GET /api/v1/apps/verify_credentials` — the application behind the bearer
/// token. Any valid token works, user-level or app-level
/// (`client_credentials`), with no scope requirement: Mastodon's controller
/// checks `valid_doorkeeper_token?` and nothing else.
pub async fn verify_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    let token = oauth::find_active_token(&state.pool, &hash_secret(bearer))
        .await?
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    let app = oauth::find_app_by_id(&state.pool, token.app_id)
        .await?
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    Ok(Json(application_json(&app)))
}

/// The API renders a rejected registration in English: this endpoint is
/// anonymous and its wording is what Mastodon clients already display.
fn unprocessable(invalid: &oauth_app::Invalid) -> ApiError {
    ApiError::Unprocessable(invalid.to_string())
}
