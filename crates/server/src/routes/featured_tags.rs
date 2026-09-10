//! `/api/v1/featured_tags*` — featured hashtags on a profile. Featuring a tag
//! federates as `Add(Hashtag)` (and unfeaturing `Remove(Hashtag)`) targeting
//! the actor's featured collection; the listing surfaces the owner's usage
//! stats for each tag. See [`super::tags`] for the `/tags/{id}/feature` aliases.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use plamenu_db::{account, featured_tag, tag};
use serde::Deserialize;
use serde_json::Value;

use super::tags::normalize_hashtag;
use crate::actions;
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{self, featured_tag_json};
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /api/v1/featured_tags` — the viewer's featured tags, busiest first.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `read` or `read:accounts` here.
    current.require_scope("read:accounts")?;
    let entities = render_for_account(&state, &current.account).await?;
    Ok(Json(Value::Array(entities)))
}

#[derive(Deserialize, Default)]
pub struct CreateParams {
    name: Option<String>,
}

/// `POST /api/v1/featured_tags` — features the named hashtag (creating it if
/// needed) and returns the new `FeaturedTag`, like Mastodon's
/// featured-tags endpoint.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let params: CreateParams = if body.is_empty() {
        CreateParams::default()
    } else {
        super::params::parse_body(&headers, &body)?
    };
    let raw = params.name.unwrap_or_default();
    // Mastodon's `CreateFeaturedTagService` raises a 422 on a blank/invalid name.
    let name = normalize_hashtag(&raw).ok_or_else(|| {
        ApiError::Unprocessable("Validation failed: Name is not a valid hashtag".into())
    })?;
    let tag_id = tag::ensure(&state.pool, &name).await?;
    actions::feature_tag(&state, &current.account, tag_id, &name).await?;
    let entity = single_featured_tag(&state, &current.account, tag_id).await?;
    Ok(Json(entity))
}

/// `DELETE /api/v1/featured_tags/{id}` — unfeatures by row id, federates the
/// `Remove`, and answers an empty object like Mastodon's `render_empty`.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    if !actions::unfeature_tag_by_id(&state, &current.account, id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(serde_json::json!({})))
}

/// `GET /api/v1/featured_tags/suggestions` — tags the viewer has used but not
/// featured, most recently used first; rendered as `Tag` entities.
pub async fn suggestions(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("read:accounts")?;
    let suggestions = featured_tag::suggestions(&state.pool, current.account.id).await?;
    // Suggestions are unfeatured by definition; `following` is resolved per tag.
    let ids: Vec<i64> = suggestions.iter().map(|s| s.tag_id).collect();
    let following = tag::followed_ids(&state.pool, current.account.id, &ids).await?;
    let featuring = std::collections::HashSet::new();
    let entities = entities::tag_json_page(
        &state.pool,
        &state.config.domain,
        &suggestions,
        &following,
        &featuring,
        true,
    )
    .await?;
    Ok(Json(Value::Array(entities)))
}

/// `GET /api/v1/accounts/{id}/featured_tags` — a given account's featured tags.
/// Public, like Mastodon's per-account featured-tags endpoint.
pub async fn account_index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(account_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if let Some(viewer) = &viewer {
        viewer.require_scope("read:accounts")?;
    }
    let account = account::find_publicly_available_by_id(&state.pool, account_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let entities = render_for_account(&state, &account).await?;
    Ok(Json(Value::Array(entities)))
}

/// Renders an account's featured tags as `FeaturedTag` entities.
async fn render_for_account(
    state: &AppState,
    account: &plamenu_db::account::Account,
) -> Result<Vec<Value>, ApiError> {
    let tags = featured_tag::list(&state.pool, account.id).await?;
    Ok(tags
        .iter()
        .map(|t| featured_tag_json(&state.config.domain, account, t))
        .collect())
}

/// The `FeaturedTag` entity for one tag the account features (after a create).
async fn single_featured_tag(
    state: &AppState,
    account: &plamenu_db::account::Account,
    tag_id: i64,
) -> Result<Value, ApiError> {
    let tags = featured_tag::list(&state.pool, account.id).await?;
    let found = tags
        .iter()
        .find(|t| t.tag_id == tag_id)
        .ok_or(ApiError::NotFound)?;
    Ok(featured_tag_json(&state.config.domain, account, found))
}
