//! Native explicit-read inbox, alongside Mastodon's marker-based endpoints.
use crate::{AppState, auth::CurrentUser, error::ApiError};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use plamenu_db::notification_read;
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Default, Deserialize)]
pub struct Page {
    #[serde(alias = "max_id")]
    pub before: Option<i64>,
    pub unread: Option<bool>,
    pub mail: Option<bool>,
    pub kind: Option<String>,
}
impl Page {
    pub fn kinds(&self) -> Result<Option<Vec<String>>, ApiError> {
        self.kind
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| {
                if super::notifications::TYPES.contains(&k.as_str()) {
                    Ok(vec![k.clone()])
                } else {
                    Err(ApiError::Unprocessable("Unknown notification type".into()))
                }
            })
            .transpose()
    }
}
fn response(value: Value) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/notifications/inbox", get(index))
        .route("/api/v1/notifications/seen", post(seen))
        .route("/api/v1/notifications/unseen", post(unseen))
        .route("/api/v1/notifications/seen_all", post(seen_all))
        .route("/api/v1/notifications/delete_bulk", post(delete_bulk))
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:notifications")?;
    let kinds = page.kinds()?;
    let mut rows = notification_read::list(
        &state.pool,
        current.user.id,
        current.account.id,
        notification_read::Filter {
            before: page.before,
            unread: page.unread.unwrap_or(false),
            mail: page.mail.unwrap_or(false),
            kinds: kinds.as_deref(),
            exclude_direct: false,
        },
    )
    .await?;
    let before = if rows.len() > 40 {
        rows.truncate(40);
        rows.last().map(|n| n.id.to_string())
    } else {
        None
    };
    let states = notification_read::read_states(
        &state.pool,
        current.user.id,
        current.account.id,
        &rows.iter().map(|n| n.id).collect::<Vec<_>>(),
    )
    .await?;
    let mut items = crate::entities::render_notifications(
        &state.pool,
        &state.config.domain,
        &rows,
        current.account.id,
    )
    .await?;
    for item in &mut items {
        if let Some(id) = item["id"].as_str().and_then(|id| id.parse::<i64>().ok()) {
            item["seen"] = json!(states.get(&id).copied().unwrap_or(false));
        }
    }
    Ok(response(json!({"items":items,"before":before})))
}
#[derive(Deserialize)]
struct Ids {
    ids: Vec<String>,
}
fn ids(input: &Ids) -> Result<Vec<i64>, ApiError> {
    if input.ids.len() > 500 {
        return Err(ApiError::Unprocessable(
            "At most 500 notification IDs".into(),
        ));
    }
    input
        .ids
        .iter()
        .map(|id| {
            id.parse::<i64>()
                .map_err(|_| ApiError::Unprocessable("Invalid notification ID".into()))
        })
        .collect()
}
async fn update(
    state: AppState,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
    read: bool,
) -> Result<Response, ApiError> {
    current.require_scope("write:notifications")?;
    let ids = ids(&super::params::parse_body(&headers, &body)?)?;
    let updated = notification_read::set_many(&state.pool, current.account.id, &ids, read).await?;
    Ok(response(json!({"updated":updated})))
}
async fn seen(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    update(state, current, headers, body, true).await
}
async fn unseen(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    update(state, current, headers, body, false).await
}
async fn seen_all(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Response, ApiError> {
    current.require_scope("write:notifications")?;
    notification_read::mark_all_read(&state.pool, current.user.id, current.account.id).await?;
    Ok(response(json!({})))
}
async fn delete_bulk(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:notifications")?;
    let ids = ids(&super::params::parse_body(&headers, &body)?)?;
    let deleted = notification_read::delete_many(&state.pool, current.account.id, &ids).await?;
    Ok(response(json!({"deleted":deleted})))
}
