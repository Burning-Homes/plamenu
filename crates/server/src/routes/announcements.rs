//! Announcements API (Mastodon's announcements endpoints and their
//! `reactions`/`dismisses` companions).
//!
//! All endpoints require a logged-in user. Listing returns the published
//! announcements; reacting and dismissing mutate the caller's own state and,
//! for reactions, broadcast an `announcement.reaction` event to every live
//! `user` stream. Creating/publishing announcements is admin-only (the
//! `plamenu announcement` CLI for now), so there is no write surface here.

use axum::Json;
use axum::extract::{Path, State};
use plamenu_db::announcement;
use serde_json::{Value, json};

use crate::actions;
use crate::auth::CurrentUser;
use crate::error::ApiError;
use crate::state::AppState;

/// Mastodon caps an announcement at 8 distinct reaction emoji
/// (`ReactionValidator::LIMIT`).
const REACTION_LIMIT: i64 = 8;

/// Renders every published announcement as its API entity for `viewer` —
/// chronological, reactions grouped, `read` resolved. Shared by the API index
/// and the web client's banner/page.
pub(crate) async fn announcements_json(
    state: &AppState,
    viewer: i64,
) -> Result<Vec<Value>, ApiError> {
    let announcements = announcement::published_chronological(&state.pool).await?;
    if announcements.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = announcements.iter().map(|a| a.id).collect();

    let reactions = announcement::reactions_for(&state.pool, &ids, Some(viewer)).await?;
    let read: std::collections::HashSet<i64> =
        announcement::muted_announcement_ids(&state.pool, viewer, &ids)
            .await?
            .into_iter()
            .collect();

    crate::entities::announcement_json_many(state, &announcements, Some(viewer), &reactions, &read)
        .await
}

/// `GET /api/v1/announcements` — the published announcements, chronological.
pub async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(Value::Array(
        announcements_json(&state, current.account.id).await?,
    )))
}

/// Adds `account_id`'s reaction to a published announcement — the shared core
/// of the API `react` handler and the web verb. Idempotent; enforces the
/// distinct-emoji cap and broadcasts the streaming event on a fresh insert.
pub(crate) async fn add_reaction(
    state: &AppState,
    account_id: i64,
    announcement_id: i64,
    name: &str,
) -> Result<(), ApiError> {
    let ann = announcement::find_published(&state.pool, announcement_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let emoji = actions::normalize_local_reaction(state, account_id, name).await?;

    // A new emoji may not push the announcement past the distinct-emoji limit.
    let existing = announcement::reaction_count(&state.pool, ann.id, &emoji.name).await?;
    if existing == 0 {
        let distinct =
            announcement::distinct_reaction_names_excluding(&state.pool, ann.id, &emoji.name)
                .await?;
        if distinct >= REACTION_LIMIT {
            return Err(ApiError::Unprocessable(
                "Validation failed: too many reactions".into(),
            ));
        }
    }

    let inserted = announcement::create_reaction(
        &state.pool,
        account_id,
        ann.id,
        &emoji.name,
        emoji.custom_emoji_id,
    )
    .await?;
    if inserted {
        if let Some(origin_id) = emoji.custom_emoji_origin_id {
            plamenu_db::custom_emoji::record_reaction_usage(&state.pool, account_id, origin_id)
                .await?;
        }
        crate::streaming::announcement_reaction(state, ann.id, &emoji.name).await;
    }
    Ok(())
}

/// Removes `account_id`'s reaction; `Err(NotFound)` when it never existed,
/// like Mastodon. Shared by the API `unreact` handler and the web verb.
pub(crate) async fn remove_reaction(
    state: &AppState,
    account_id: i64,
    announcement_id: i64,
    name: &str,
) -> Result<(), ApiError> {
    let ann = announcement::find_published(&state.pool, announcement_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let emoji = actions::normalize_local_reaction(state, account_id, name).await?;
    let removed = if let Some(emoji_id) = emoji.custom_emoji_id {
        announcement::delete_reaction_by_emoji_id(&state.pool, account_id, ann.id, emoji_id).await?
    } else {
        announcement::delete_reaction(&state.pool, account_id, ann.id, &emoji.name).await?
    };
    if !removed {
        return Err(ApiError::NotFound);
    }
    crate::streaming::announcement_reaction(state, ann.id, &emoji.name).await;
    Ok(())
}

/// Marks a published announcement read for `account_id` — the shared core of
/// the API `dismiss` handler and the web verb.
pub(crate) async fn mark_read(
    state: &AppState,
    account_id: i64,
    announcement_id: i64,
) -> Result<(), ApiError> {
    let ann = announcement::find_published(&state.pool, announcement_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    announcement::mute(&state.pool, account_id, ann.id).await?;
    Ok(())
}

/// `PUT /api/v1/announcements/{id}/reactions/{name}` — add the caller's
/// reaction. Idempotent; a brand-new emoji past the per-announcement limit is
/// rejected like Mastodon's `ReactionValidator`.
pub async fn react(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    add_reaction(&state, current.account.id, id, &name).await?;
    Ok(Json(json!({})))
}

/// `DELETE /api/v1/announcements/{id}/reactions/{name}` — remove the caller's
/// reaction. A reaction the caller never made is a `404`, like Mastodon.
pub async fn unreact(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:favourites")?;
    remove_reaction(&state, current.account.id, id, &name).await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/announcements/{id}/dismiss` — mark the announcement read for
/// the caller.
pub async fn dismiss(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    mark_read(&state, current.account.id, id).await?;
    Ok(Json(json!({})))
}
