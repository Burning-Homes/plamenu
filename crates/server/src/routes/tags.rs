//! `/api/v1/tags/{id}` and `/api/v1/followed_tags` — hashtag following.
//! Following a tag is purely local: it injects the tag's public, non-reblog
//! statuses into the follower's home timeline (`status::home_timeline`).

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use plamenu_db::{featured_tag, tag};
use serde::Deserialize;
use serde_json::Value;

use crate::actions;
use crate::auth::{CurrentUser, MaybeUser};
use crate::entities::{self, tag_json_absent};
use crate::error::ApiError;
use crate::state::AppState;

/// Mastodon's default and (via `limit_param`) doubled maximum page size.
const FOLLOWED_TAGS_LIMIT: i64 = 100;

/// Mastodon's `Tag.find_normalized` + `HASHTAG_NAME_RE` gate, narrowed to the
/// subset Plamenu recognises: strip a leading `#`/`＃`, then require a
/// non-empty run of word characters or separators (`_`, `·`) containing at
/// least one letter. Returns the normalized name, or `None` for an invalid
/// hashtag (the controllers answer 404).
pub(crate) fn normalize_hashtag(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start_matches(['#', '＃']);
    if trimmed.is_empty() {
        return None;
    }
    let mut has_alpha = false;
    for c in trimmed.chars() {
        if c.is_alphabetic() {
            has_alpha = true;
        } else if !(c.is_alphanumeric() || c == '_' || c == '·') {
            return None;
        }
    }
    has_alpha.then(|| trimmed.to_owned())
}

/// `GET /api/v1/tags/{id}` — the `Tag` entity. Optional auth, like Mastodon;
/// `following` is set only for an authenticated viewer. A well-formed but
/// unknown tag renders as a never-followed tag rather than 404ing.
pub async fn show(
    State(state): State<AppState>,
    viewer: MaybeUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let viewer = viewer.require_preview(state.timeline_preview_tag().await)?;
    let name = normalize_hashtag(&id).ok_or(ApiError::NotFound)?;
    let domain = &state.config.domain;
    let entity = match (viewer, tag::find_by_name(&state.pool, &name).await?) {
        (Some(user), Some(found)) => {
            let following = tag::is_following(&state.pool, user.account.id, found.id).await?;
            let featuring = featured_tag::exists(&state.pool, user.account.id, found.id).await?;
            entities::tag_json(
                &state.pool,
                domain,
                &found,
                Some(following),
                Some(featuring),
            )
            .await?
        }
        (Some(_), None) => tag_json_absent(domain, &name, Some(false), Some(false)),
        (None, Some(found)) => entities::tag_json(&state.pool, domain, &found, None, None).await?,
        (None, None) => tag_json_absent(domain, &name, None, None),
    };
    Ok(Json(entity))
}

/// `POST /api/v1/tags/{id}/follow` — creates the tag if needed, like
/// Mastodon's `set_or_create_tag`. Idempotent.
pub async fn follow(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `follow`, `write` or `write:follows` here.
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let name = normalize_hashtag(&id).ok_or(ApiError::NotFound)?;
    let tag_id = tag::ensure(&state.pool, &name).await?;
    tag::follow(&state.pool, current.account.id, tag_id).await?;
    let found = tag::find_by_name(&state.pool, &name)
        .await?
        .ok_or(ApiError::NotFound)?;
    let featuring = featured_tag::exists(&state.pool, current.account.id, found.id).await?;
    Ok(Json(
        entities::tag_json(
            &state.pool,
            &state.config.domain,
            &found,
            Some(true),
            Some(featuring),
        )
        .await?,
    ))
}

/// `POST /api/v1/tags/{id}/unfollow` — idempotent; unknown tags just answer
/// the not-following entity.
pub async fn unfollow(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if current.require_scope("write:follows").is_err() {
        current.require_scope("follow")?;
    }
    let name = normalize_hashtag(&id).ok_or(ApiError::NotFound)?;
    let domain = &state.config.domain;
    let entity = if let Some(found) = tag::find_by_name(&state.pool, &name).await? {
        tag::unfollow(&state.pool, current.account.id, found.id).await?;
        let featuring = featured_tag::exists(&state.pool, current.account.id, found.id).await?;
        entities::tag_json(&state.pool, domain, &found, Some(false), Some(featuring)).await?
    } else {
        tag_json_absent(domain, &name, Some(false), Some(false))
    };
    Ok(Json(entity))
}

/// `POST /api/v1/tags/{id}/feature` — features the hashtag on the viewer's
/// profile (creating it if needed) and federates `Add(Hashtag)`. Returns the
/// `Tag` entity, like Mastodon's tag-feature endpoint.
pub async fn feature(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // Mastodon accepts `write` or `write:accounts` here.
    current.require_scope("write:accounts")?;
    let name = normalize_hashtag(&id).ok_or(ApiError::NotFound)?;
    let tag_id = tag::ensure(&state.pool, &name).await?;
    actions::feature_tag(&state, &current.account, tag_id, &name).await?;
    let following = tag::is_following(&state.pool, current.account.id, tag_id).await?;
    let found = tag::find_by_name(&state.pool, &name)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        entities::tag_json(
            &state.pool,
            &state.config.domain,
            &found,
            Some(following),
            Some(true),
        )
        .await?,
    ))
}

/// `POST /api/v1/tags/{id}/unfeature` — idempotent; unknown tags just answer
/// the `Tag` entity.
pub async fn unfeature(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:accounts")?;
    let name = normalize_hashtag(&id).ok_or(ApiError::NotFound)?;
    let domain = &state.config.domain;
    let entity = if let Some(found) = tag::find_by_name(&state.pool, &name).await? {
        let following = tag::is_following(&state.pool, current.account.id, found.id).await?;
        actions::unfeature_tag(&state, &current.account, found.id, &name).await?;
        entities::tag_json(&state.pool, domain, &found, Some(following), Some(false)).await?
    } else {
        tag_json_absent(domain, &name, Some(false), Some(false))
    };
    Ok(Json(entity))
}

#[derive(Deserialize)]
pub struct FollowedQuery {
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: Option<i64>,
}

/// `GET /api/v1/followed_tags` — the viewer's followed tags, newest follow
/// first, keyset-paginated by `TagFollow` id (the `Link` ids are follow ids,
/// not tag ids, like Mastodon).
pub async fn followed_index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(query): Query<FollowedQuery>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    // Mastodon accepts `follow`, `read` or `read:follows`.
    if current.require_scope("read:follows").is_err() {
        current.require_scope("follow")?;
    }
    let limit = query
        .limit
        .unwrap_or(FOLLOWED_TAGS_LIMIT)
        .clamp(1, FOLLOWED_TAGS_LIMIT * 2);
    let rows = tag::followed(
        &state.pool,
        current.account.id,
        query.max_id,
        query.since_id,
        query.min_id,
        limit,
    )
    .await?;
    // All rows are followed by definition; resolve featuring for the page.
    let following: HashSet<i64> = rows.iter().map(|row| row.id).collect();
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let featuring = featured_tag::featured_ids(&state.pool, current.account.id, &ids).await?;
    let entities = entities::tag_json_page(
        &state.pool,
        &state.config.domain,
        &rows,
        &following,
        &featuring,
        true,
    )
    .await?;

    let mut links = Vec::new();
    if rows.len() == usize::try_from(limit).unwrap_or(usize::MAX)
        && let Some(last) = rows.last()
    {
        links.push(format!(
            "<https://{}/api/v1/followed_tags?limit={limit}&max_id={}>; rel=\"next\"",
            state.config.domain, last.follow_id
        ));
    }
    if let Some(first) = rows.first() {
        links.push(format!(
            "<https://{}/api/v1/followed_tags?limit={limit}&since_id={}>; rel=\"prev\"",
            state.config.domain, first.follow_id
        ));
    }
    let mut headers = HeaderMap::new();
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        headers.insert(header::LINK, value);
    }
    Ok((headers, Json(Value::Array(entities))))
}
