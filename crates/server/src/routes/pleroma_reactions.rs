//! Pleroma-compatible emoji reaction API.
//!
//! Mastodon has no equivalent, but Pleroma/Akkoma clients use this surface to
//! list reactors and to create/delete `EmojiReact` activities.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, State};
use plamenu_db::{account, block, reaction, status};
use serde_json::{Value, json};

use crate::actions;
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{can_view, displayed_reaction_name, render_status};
use crate::error::ApiError;
use crate::state::AppState;

fn reaction_filter_name(raw: &str) -> String {
    raw.strip_prefix(':')
        .and_then(|rest| rest.strip_suffix(':'))
        .unwrap_or(raw)
        .to_owned()
}

async fn visible_reaction_groups(
    state: &AppState,
    status_id: i64,
    viewer_id: Option<i64>,
    emoji: Option<&str>,
) -> Result<Vec<reaction::ReactionGroup>, ApiError> {
    let mut groups = reaction::for_statuses(&state.pool, &[status_id])
        .await?
        .remove(&status_id)
        .unwrap_or_default();
    if let Some(raw) = emoji {
        let name = reaction_filter_name(raw);
        // Clients echo the displayed name back, so a remote custom emoji is
        // addressable by its qualified `shortcode@host` form too.
        groups.retain(|group| {
            group.name == name || displayed_reaction_name(&state.config.domain, group) == name
        });
    }
    let hidden: HashSet<i64> = match viewer_id {
        Some(viewer_id) => {
            let ids: Vec<i64> = groups
                .iter()
                .flat_map(|group| group.account_ids.iter().copied())
                .collect();
            block::hidden_authors(&state.pool, viewer_id, &ids)
                .await?
                .into_iter()
                .collect()
        }
        None => HashSet::new(),
    };
    for group in &mut groups {
        group.account_ids.retain(|id| !hidden.contains(id));
        group.count = i64::try_from(group.account_ids.len()).unwrap_or(i64::MAX);
    }
    groups.retain(|group| group.count > 0);
    Ok(groups)
}

async fn reaction_groups_json(
    state: &AppState,
    groups: &[reaction::ReactionGroup],
    viewer_id: Option<i64>,
) -> Result<Value, ApiError> {
    let account_ids: Vec<i64> = groups
        .iter()
        .flat_map(|group| group.account_ids.iter().copied())
        .collect();
    let accounts = account::find_by_ids(&state.pool, &account_ids).await?;
    // Render every distinct reactor once, then index by id so each group's
    // account list is assembled without a per-reactor query.
    let rendered =
        crate::entities::render_accounts(&state.pool, &state.config.domain, &accounts, viewer_id)
            .await?;
    let rendered_by_id: HashMap<i64, Value> = accounts
        .iter()
        .map(|account| account.id)
        .zip(rendered)
        .collect();

    let mut entries = Vec::with_capacity(groups.len());
    for group in groups {
        let mut rendered_accounts = Vec::with_capacity(group.account_ids.len());
        for account_id in &group.account_ids {
            if let Some(rendered) = rendered_by_id.get(account_id) {
                rendered_accounts.push(rendered.clone());
            }
        }
        let me = viewer_id.is_some_and(|viewer| group.account_ids.contains(&viewer));
        entries.push(json!({
            "name": displayed_reaction_name(&state.config.domain, group),
            "count": rendered_accounts.len(),
            "accounts": rendered_accounts,
            "url": group.custom_emoji_url,
            "me": me,
        }));
    }
    Ok(Value::Array(entries))
}

async fn index_inner(
    state: AppState,
    viewer: Option<CurrentUser>,
    status_id: i64,
    emoji: Option<String>,
) -> Result<Json<Value>, ApiError> {
    let viewer_id = viewer.as_ref().map(|user| user.account.id);
    let item = status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !can_view(&state.pool, &item, viewer_id).await? {
        return Err(ApiError::NotFound);
    }
    let groups = visible_reaction_groups(&state, status_id, viewer_id, emoji.as_deref()).await?;
    Ok(Json(
        reaction_groups_json(&state, &groups, viewer_id).await?,
    ))
}

/// `GET /api/v1/pleroma/statuses/{id}/reactions`.
pub async fn index(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path(status_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    index_inner(state, viewer, status_id, None).await
}

/// `GET /api/v1/pleroma/statuses/{id}/reactions/{emoji}`.
pub async fn index_emoji(
    State(state): State<AppState>,
    MaybeUser(viewer): MaybeUser,
    Path((status_id, emoji)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    index_inner(state, viewer, status_id, Some(emoji)).await
}

/// `PUT /api/v1/pleroma/statuses/{id}/reactions/{emoji}`.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((status_id, emoji)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write")?;
    let item = actions::react_with_emoji(&state, &current.account, status_id, &emoji).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}

/// `DELETE /api/v1/pleroma/statuses/{id}/reactions/{emoji}`.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((status_id, emoji)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write")?;
    let item = actions::unreact_with_emoji(&state, &current.account, status_id, &emoji).await?;
    Ok(Json(
        render_status(
            &state.pool,
            &state.config.domain,
            &item,
            Some(current.account.id),
        )
        .await?,
    ))
}
