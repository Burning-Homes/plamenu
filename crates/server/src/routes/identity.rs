//! Plamenu's FEP-c390 API (independent of Mastodon's retired Keybase API).

use crate::{AppState, auth::CurrentUser, error::ApiError};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::Value;

pub async fn list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let account = plamenu_db::account::find_publicly_available_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if plamenu_db::account::is_internal(&state.pool, id).await?
        || !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account)
            .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(crate::identity::list(&state, &account).await?))
}

pub async fn publish(
    State(state): State<AppState>,
    current: CurrentUser,
    Json(document): Json<Value>,
) -> Result<Json<Vec<Value>>, ApiError> {
    current.require_scope("write:accounts")?;
    Ok(Json(
        crate::identity::publish(&state, current.account.id, document).await?,
    ))
}

#[derive(Deserialize)]
pub struct Removal {
    pub subject: String,
}

pub async fn remove(
    State(state): State<AppState>,
    current: CurrentUser,
    Json(input): Json<Removal>,
) -> Result<Json<Vec<Value>>, ApiError> {
    current.require_scope("write:accounts")?;
    Ok(Json(
        crate::identity::remove(&state, current.account.id, &input.subject).await?,
    ))
}
