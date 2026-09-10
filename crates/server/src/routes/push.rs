//! `/api/v1/push/subscription` — Web Push registration, Mastodon's
//! semantics: one subscription per access token (`POST` replaces it, `GET`
//! and `PUT` address the token's own, `DELETE` removes it), `push` scope
//! required everywhere, and Rails' exact validation wording.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use plamenu_db::web_push::{self, NewSubscription, Subscription};
use serde_json::{Map, Value, json};

use super::notifications::TYPES;
use crate::auth::CurrentUser;
use crate::error::ApiError;
use crate::state::AppState;
use crate::web_push::parse_client_keys;

/// The request body as a JSON value: native JSON, or a Rails-style
/// bracketed form (`subscription[keys][auth]=…`) folded into one.
pub(super) fn body_to_value(headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if content_type.starts_with("application/json") {
        return serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON body: {e}")));
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(body)
        .map_err(|e| ApiError::BadRequest(format!("invalid form body: {e}")))?;
    let mut root = Map::new();
    for (key, value) in pairs {
        // "a[b][c]" → path ["a", "b", "c"].
        let path: Vec<&str> = key.split(['[', ']']).filter(|s| !s.is_empty()).collect();
        if path.is_empty() {
            continue;
        }
        let mut node = &mut root;
        for segment in &path[..path.len() - 1] {
            node = node
                .entry((*segment).to_owned())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| ApiError::BadRequest(format!("conflicting form key {key}")))?;
        }
        node.insert(path[path.len() - 1].to_owned(), Value::String(value));
    }
    Ok(Value::Object(root))
}

/// Rails' `ActiveModel::Type::Boolean` cast over a JSON value.
fn cast_bool(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => super::params::truthy(Some(s)),
        _ => false,
    }
}

/// Mastodon's `data_params`: `policy` plus `alerts` filtered to the known
/// notification types, alert values cast to booleans. Empty `data` is `{}`.
pub(super) fn data_params(root: &Value) -> Value {
    let Some(data) = root.get("data").and_then(Value::as_object) else {
        return json!({});
    };
    let mut out = Map::new();
    if let Some(policy) = data.get("policy").and_then(Value::as_str) {
        out.insert("policy".to_owned(), Value::String(policy.to_owned()));
    }
    if let Some(alerts) = data.get("alerts").and_then(Value::as_object) {
        let mut cast = Map::new();
        for kind in TYPES {
            if let Some(value) = alerts.get(*kind) {
                cast.insert((*kind).to_owned(), Value::Bool(cast_bool(value)));
            }
        }
        out.insert("alerts".to_owned(), Value::Object(cast));
    }
    Value::Object(out)
}

/// Rails' `URLValidator`: parsable, http(s), with a host.
fn valid_url(value: &str) -> bool {
    value
        .parse::<axum::http::Uri>()
        .is_ok_and(|uri| matches!(uri.scheme_str(), Some("http" | "https")) && uri.host().is_some())
}

/// The `WebPushSubscription` entity. `id` is a bare integer — the one
/// Mastodon entity whose id is not a string.
pub(super) fn entity(subscription: &Subscription, server_key: &str) -> Value {
    let alerts: Map<String, Value> = subscription
        .alert_kinds
        .iter()
        .zip(&subscription.alert_values)
        .map(|(kind, enabled)| (kind.clone(), Value::Bool(*enabled)))
        .collect();
    json!({
        "id": subscription.id,
        "endpoint": subscription.endpoint,
        "standard": subscription.standard,
        "alerts": Value::Object(alerts),
        "server_key": server_key,
        "policy": subscription.policy,
    })
}

pub(super) async fn server_key(state: &AppState) -> Result<String, ApiError> {
    let vapid = crate::web_push::vapid(state)
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    Ok(vapid.public_key)
}

pub(super) struct ParsedSubscription {
    pub endpoint: String,
    pub standard: bool,
    pub p256dh: String,
    pub auth: String,
}

/// Parses and validates the `subscription` object shared by the OAuth push
/// endpoint and Mastodon's browser-session `/api/web/push_subscriptions`.
pub(super) fn parse_subscription(root: &Value) -> Result<ParsedSubscription, ApiError> {
    let missing =
        |key: &str| ApiError::BadRequest(format!("param is missing or its value is empty: {key}"));
    let subscription = root
        .get("subscription")
        .and_then(Value::as_object)
        .filter(|o| !o.is_empty())
        .ok_or_else(|| missing("subscription"))?;
    let keys = subscription
        .get("keys")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("keys"))?;
    let endpoint = subscription
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let standard = subscription.get("standard").is_some_and(cast_bool);
    let p256dh = keys
        .get("p256dh")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth = keys.get("auth").and_then(Value::as_str).unwrap_or_default();

    // Mastodon's model validations, collected in declaration order.
    let mut errors: Vec<&str> = Vec::new();
    if endpoint.is_empty() {
        errors.push("Endpoint can't be blank");
    }
    if !valid_url(endpoint) {
        errors.push("Endpoint is invalid");
    }
    if p256dh.is_empty() {
        errors.push("Key p256dh can't be blank");
    }
    if auth.is_empty() {
        errors.push("Key auth can't be blank");
    }
    if parse_client_keys(p256dh, auth).is_none() {
        // `WebPushKeyValidator`'s message, odd as it is for P-256 keys.
        errors.push("is not a valid Ed25519 or Curve25519 key");
    }
    if !errors.is_empty() {
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }

    Ok(ParsedSubscription {
        endpoint: endpoint.to_owned(),
        standard,
        p256dh: p256dh.to_owned(),
        auth: auth.to_owned(),
    })
}

/// `POST /api/v1/push/subscription` — replaces the token's subscription.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("push")?;
    // The extractor already validated this token; keep the raw value — it
    // travels inside every encrypted push payload, like Mastodon's.
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Unauthorized(crate::auth::INVALID_TOKEN.into()))?
        .to_owned();

    let root = body_to_value(&headers, &body)?;
    let subscription = parse_subscription(&root)?;
    let data = data_params(&root);
    let saved = web_push::replace_for_token(
        &state.pool,
        NewSubscription {
            user_id: current.user.id,
            access_token_id: current.token_id,
            access_token: &bearer,
            endpoint: &subscription.endpoint,
            key_p256dh: &subscription.p256dh,
            key_auth: &subscription.auth,
            standard: subscription.standard,
            data: &data,
        },
    )
    .await?;
    Ok(Json(entity(&saved, &server_key(&state).await?)))
}

/// `GET /api/v1/push/subscription` — the token's subscription, or 404.
pub async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("push")?;
    let subscription = web_push::find_for_token(&state.pool, current.token_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(entity(&subscription, &server_key(&state).await?)))
}

/// `PUT /api/v1/push/subscription` — replaces `data` (alerts + policy).
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("push")?;
    let root = body_to_value(&headers, &body)?;
    let data = data_params(&root);
    let subscription = web_push::update_data(&state.pool, current.token_id, &data)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(entity(&subscription, &server_key(&state).await?)))
}

/// `DELETE /api/v1/push/subscription` — removes the token's subscription
/// (idempotent, answers `{}` like Mastodon's `render_empty`).
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("push")?;
    web_push::delete_for_token(&state.pool, current.token_id).await?;
    Ok(Json(json!({})))
}
