//! The profile directory: `GET /api/v1/directory`, Mastodon's
//! opt-in listing of discoverable accounts. Anonymous-readable, 404 when
//! the operator turns the directory off.

use axum::Json;
use axum::extract::{Query, State};
use plamenu_db::account;
use plamenu_db::discovery::{self, DirectoryPage};
use serde::Deserialize;
use serde_json::Value;

use super::params;
use crate::AppState;
use crate::auth::MaybeUser;
use crate::entities::render_accounts;
use crate::error::ApiError;

/// Mastodon's `DEFAULT_ACCOUNTS_LIMIT`; `limit_param` allows up to double.
const DIRECTORY_LIMIT: i64 = 40;

#[derive(Debug, Default, Deserialize)]
pub struct DirectoryQuery {
    order: Option<String>,
    local: Option<String>,
    offset: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/directory` — one offset-paged slice of the discoverable
/// accounts, most recently posted first (`order=active`, the default) or
/// newest account first (`order=new`). Signed-in viewers don't see accounts
/// or domains they block or mute.
pub async fn directory(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Query(query): Query<DirectoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    if !settings.profile_directory {
        return Err(ApiError::NotFound);
    }
    let page = DirectoryPage {
        order_new: query.order.as_deref() == Some("new"),
        local_only: params::truthy(query.local.as_deref()),
        viewer_id: viewer.map(|user| user.account.id),
        limit: query
            .limit
            .unwrap_or(DIRECTORY_LIMIT)
            .clamp(1, DIRECTORY_LIMIT * 2),
        offset: query.offset.unwrap_or(0).max(0),
    };
    let ids = discovery::directory_account_ids(&state.pool, &page).await?;
    let mut listed = account::find_by_ids(&state.pool, &ids).await?;
    // Preserve the directory's ranked order; drop any id that vanished
    // between the id scan and the fetch, as the per-row lookup did.
    listed.sort_by_key(|a| ids.iter().position(|id| *id == a.id).unwrap_or(usize::MAX));
    let entities =
        render_accounts(&state.pool, &state.config.domain, &listed, page.viewer_id).await?;
    Ok(Json(Value::Array(entities)))
}
