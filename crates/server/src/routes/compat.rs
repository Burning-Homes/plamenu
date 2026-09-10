//! Client-bootstrap endpoints. Mastodon apps (Phanpy, Elk, Tusky, the
//! official app) call these on launch and some treat a 404 as fatal, so
//! they must exist even while the features behind them don't: each one
//! serves the exact Mastodon-shaped response for a server that has no
//! followed tags / trends — an empty collection — and `preferences` serves a
//! fresh account's defaults.
//!
//! Auth and scope rules mirror the corresponding Mastodon controllers.

use axum::Json;
use axum::extract::State;
use plamenu_db::user;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::CurrentUser;
use crate::error::ApiError;

/// `GET /api/v1/custom_emojis` — the picker listing (local, enabled,
/// `visible_in_picker` emoji). Public, like Mastodon's.
pub async fn custom_emojis(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let listed = plamenu_db::custom_emoji::listed(&state.pool).await?;
    let entities: Vec<Value> = listed
        .iter()
        .map(|emoji| crate::emoji::custom_emoji_json(&state.config.domain, emoji, false))
        .collect();
    Ok(Json(json!(entities)))
}

/// `GET /api/v1/preferences` — `read` / `read:accounts`. Stored per-user
/// preferences are resolved into Mastodon's response shape; the `default`
/// visibility sentinel follows the account's `locked` flag.
pub async fn preferences(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let settings = user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    Ok(Json(settings.preferences_json(current.account.locked)))
}
