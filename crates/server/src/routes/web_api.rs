//! Mastodon's browser-session `/api/web/*` compatibility surface.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use plamenu_db::web_push::NewSubscription;
use plamenu_db::{account, status, web_push, web_setting};
use serde_json::{Map, Value, json};

use super::notifications::TYPES;
use crate::auth::{CurrentUser, INVALID_TOKEN, user_for_token};
use crate::entities::{can_view, status_web_url};
use crate::error::ApiError;
use crate::state::AppState;

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn current_with_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(CurrentUser, String), ApiError> {
    let token = bearer(headers)
        .or_else(|| crate::web::session::raw_session_token(headers))
        .ok_or_else(|| ApiError::Unauthorized(INVALID_TOKEN.into()))?;
    let current = user_for_token(state, token).await?;
    Ok((current, token.to_owned()))
}

async fn maybe_current(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<CurrentUser>, ApiError> {
    let Some(token) = bearer(headers).or_else(|| crate::web::session::raw_session_token(headers))
    else {
        return Ok(None);
    };
    user_for_token(state, token).await.map(Some)
}

/// `PATCH /api/web/settings` — store the web client's raw settings blob.
pub async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (current, _) = current_with_token(&state, &headers).await?;
    let root = super::push::body_to_value(&headers, &body)?;
    let data = root.get("data").ok_or_else(|| {
        ApiError::BadRequest("param is missing or its value is empty: data".into())
    })?;
    web_setting::upsert(&state.pool, current.user.id, data).await?;
    Ok(Json(json!({})))
}

fn default_push_data() -> Value {
    let mut alerts = Map::new();
    for kind in TYPES {
        alerts.insert((*kind).to_owned(), Value::Bool(false));
    }
    json!({ "policy": "all", "alerts": alerts })
}

fn merged_push_data(root: &Value) -> Value {
    let mut data = default_push_data();
    let supplied = super::push::data_params(root);
    if let Some(policy) = supplied.get("policy").cloned() {
        data["policy"] = policy;
    }
    if let Some(alerts) = supplied.get("alerts").and_then(Value::as_object)
        && let Some(target) = data.get_mut("alerts").and_then(Value::as_object_mut)
    {
        for (kind, value) in alerts {
            target.insert(kind.clone(), value.clone());
        }
    }
    data
}

/// `POST /api/web/push_subscriptions` — create the browser session's push
/// subscription, replacing any earlier subscription for the same token.
pub async fn push_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (current, raw_token) = current_with_token(&state, &headers).await?;
    let root = super::push::body_to_value(&headers, &body)?;
    let subscription = super::push::parse_subscription(&root)?;
    let data = merged_push_data(&root);
    let saved = web_push::replace_for_token(
        &state.pool,
        NewSubscription {
            user_id: current.user.id,
            access_token_id: current.token_id,
            access_token: &raw_token,
            endpoint: &subscription.endpoint,
            key_p256dh: &subscription.p256dh,
            key_auth: &subscription.auth,
            standard: subscription.standard,
            data: &data,
        },
    )
    .await?;
    Ok(Json(super::push::entity(
        &saved,
        &super::push::server_key(&state).await?,
    )))
}

/// `PATCH /api/web/push_subscriptions/{id}` — update alerts/policy on a
/// browser-session subscription.
pub async fn push_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (current, _) = current_with_token(&state, &headers).await?;
    let root = super::push::body_to_value(&headers, &body)?;
    let data = super::push::data_params(&root);
    let saved = web_push::update_data_for_user_by_id(&state.pool, current.user.id, id, &data)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(super::push::entity(
        &saved,
        &super::push::server_key(&state).await?,
    )))
}

/// `DELETE /api/web/push_subscriptions/{id}` — delete one of the current
/// user's browser-session subscriptions.
pub async fn push_destroy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let (current, _) = current_with_token(&state, &headers).await?;
    web_push::delete_for_user_by_id(&state.pool, current.user.id, id).await?;
    Ok(Json(json!({})))
}

/// `GET /api/web/embeds/{id}` — oEmbed JSON for a visible local status.
pub async fn embed(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let viewer = maybe_current(&state, &headers).await?;
    let viewer_id = viewer.as_ref().map(|user| user.account.id);
    let item = status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, viewer_id).await? || item.uri.is_some() {
        return Err(ApiError::NotFound);
    }
    let author = account::find_by_id(&state.pool, item.account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(oembed_entity(
        &state.config.domain,
        &item,
        &author,
        400,
        None,
    )))
}

/// The oEmbed document for a local status — Mastodon's `OEmbedSerializer`,
/// shared by `/api/web/embeds/{id}` and `GET /api/oembed`.
pub fn oembed_entity(
    domain: &str,
    item: &status::Status,
    author: &account::Account,
    width: i64,
    height: Option<i64>,
) -> Value {
    let status_url = status_web_url(domain, item, &author.username);
    let author_url = crate::entities::account_web_url(domain, author);
    let author_name = if author.display_name.trim().is_empty() {
        author.username.clone()
    } else {
        author.display_name.clone()
    };
    let escaped_status_url = plamenu_ap::text::escape_html(&status_url);
    let escaped_author = plamenu_ap::text::escape_html(&author.username);
    let html = format!(
        r#"<blockquote class="plamenu-embed" data-embed-url="{escaped_status_url}/embed"><a href="{escaped_status_url}" target="_blank" rel="noopener noreferrer">Post by @{escaped_author}</a></blockquote>"#
    );
    json!({
        "type": "rich",
        "version": "1.0",
        "author_name": author_name,
        "author_url": author_url,
        "provider_name": domain,
        "provider_url": format!("https://{domain}"),
        "cache_age": 86400,
        "html": html,
        "width": width,
        "height": height,
    })
}
