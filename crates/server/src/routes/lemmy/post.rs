use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::actions::{DeleteMode, EditParams, PostParams};
use crate::compose::PostFormat;

use super::auth::{LemmyUser, MaybeLemmyUser};
use super::entities::{community, person, post_view, post_view_with_read};
use super::error::LemmyError;
use super::ids::{resolve_required, resolve_required_many};
use plamenu_db::lemmy_id::Kind;

async fn community_of_status(
    state: &AppState,
    status: &plamenu_db::status::Status,
) -> Result<plamenu_db::account::Account, LemmyError> {
    crate::groups::communities_of_status(state, status)
        .await
        .map_err(LemmyError::from)?
        .into_iter()
        .next()
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
}

#[derive(Deserialize)]
pub struct CreatePost {
    name: String,
    community_id: i32,
    url: Option<String>,
    body: Option<String>,
    #[serde(default)]
    nsfw: bool,
}

pub async fn create(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreatePost>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let group = plamenu_db::account::find_by_id(&state.pool, community_id)
        .await?
        .filter(plamenu_db::account::Account::is_group)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let (stored, _) = crate::actions::post_status_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            text: form.body.as_deref().unwrap_or_default(),
            visibility: "public",
            in_reply_to_id: None,
            media_ids: &[],
            quoted_status_id: None,
            spoiler_text: "",
            sensitive: form.nsfw,
            language: Some("en"),
            content_type: PostFormat::Markdown,
            poll: None,
            quote_approval_policy: None,
            group_id: Some(group.id),
            title: Some(&form.name),
            external_url: form.url.as_deref(),
            event: None,
            kind: plamenu_ap::activity::PostKind::Note,
        },
        current.app_id,
    )
    .await
    .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "post_view": post_view(&state, &stored, &group, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct EditPost {
    post_id: i32,
    name: Option<String>,
    url: Option<String>,
    body: Option<String>,
    nsfw: Option<bool>,
    language_id: Option<i32>,
    custom_thumbnail: Option<String>,
    alt_text: Option<String>,
    poll: Option<Value>,
    event: Option<Value>,
}

/// Edit the Plamenu status underlying a Lemmy post.  The native edit service
/// owns validation, edit history and `ActivityPub` `Update` fan-out.
pub async fn edit(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<EditPost>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let stored = plamenu_db::status::find_by_id(&state.pool, post_id)
        .await?
        .filter(|status| status.in_reply_to_id.is_none() && status.reblog_of_id.is_none())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
    if form
        .url
        .as_deref()
        .map(str::trim)
        .map(|url| (!url.is_empty()).then_some(url))
        != form.url.as_ref().map(|_| stored.external_url.as_deref())
        || form.custom_thumbnail.is_some()
    {
        return Err(LemmyError::bad_request("unsupported_post_url_edit"));
    }
    if form.alt_text.is_some() || form.poll.is_some() || form.event.is_some() {
        return Err(LemmyError::bad_request("unsupported_post_kind_edit"));
    }
    let language = match form.language_id {
        None | Some(0 | 1) => None,
        Some(_) => return Err(LemmyError::bad_request("couldnt_find_language")),
    };
    let edited = crate::actions::edit_status(
        &state,
        &current.account,
        stored.id,
        EditParams {
            title: form.name.as_deref(),
            text: form.body.as_deref(),
            content_type: form.body.as_ref().map(|_| PostFormat::Markdown),
            sensitive: form.nsfw,
            language,
            ..EditParams::default()
        },
    )
    .await
    .map_err(LemmyError::from)?;
    let group = community_of_status(&state, &edited).await?;
    Ok(Json(json!({
        "post_view": post_view(&state, &edited, &group, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct GetPost {
    id: i32,
}

pub async fn get(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<GetPost>,
) -> Result<Json<Value>, LemmyError> {
    let id = resolve_required(&state, Kind::Status, query.id, "couldnt_find_post").await?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .filter(|status| status.in_reply_to_id.is_none() && status.reblog_of_id.is_none())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
    let group = community_of_status(&state, &status).await?;
    let viewer = current.as_ref().map(|user| user.account.id);
    let moderators = plamenu_db::group::elevated(&state.pool, group.id).await?;
    let mod_accounts = plamenu_db::account::find_by_ids(
        &state.pool,
        &moderators
            .iter()
            .map(|entry| entry.account_id)
            .collect::<Vec<_>>(),
    )
    .await?;
    let group_entity = community(&state, &group, viewer).await?;
    let mut mod_views = Vec::new();
    for account in &mod_accounts {
        mod_views.push(json!({
            "community": group_entity["community"],
            "moderator": person(&state, account, false).await?["person"],
        }));
    }
    Ok(Json(json!({
        "post_view": post_view(&state, &status, &group, viewer).await?,
        "community_view": group_entity,
        "moderators": mod_views,
        "cross_posts": [],
    })))
}

#[derive(Default, Deserialize)]
pub struct GetPosts {
    type_: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
    community_id: Option<i32>,
    community_name: Option<String>,
    sort: Option<String>,
    #[serde(default)]
    saved_only: bool,
    #[serde(default)]
    liked_only: bool,
    #[serde(default)]
    disliked_only: bool,
    show_nsfw: Option<bool>,
    page_cursor: Option<String>,
}

async fn requested_community(
    state: &AppState,
    query: &GetPosts,
) -> Result<Option<plamenu_db::account::Account>, LemmyError> {
    if let Some(id) = query.community_id {
        let id = resolve_required(state, Kind::Account, id, "couldnt_find_community").await?;
        return plamenu_db::account::find_by_id(&state.pool, id)
            .await?
            .filter(plamenu_db::account::Account::is_group)
            .map(Some)
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"));
    }
    let Some(name) = query.community_name.as_deref() else {
        return Ok(None);
    };
    let name = name.trim().strip_prefix('!').unwrap_or(name.trim());
    let account = if let Some((username, domain)) = name.rsplit_once('@') {
        if state.config.is_local_domain(domain) {
            plamenu_db::account::find_local_by_username(&state.pool, username)
                .await?
                .filter(plamenu_db::account::Account::is_group)
        } else {
            plamenu_db::account::find_remote_group_by_acct(&state.pool, username, domain).await?
        }
    } else {
        plamenu_db::account::find_local_by_username(&state.pool, name)
            .await?
            .filter(plamenu_db::account::Account::is_group)
    };
    account
        .map(Some)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
}

fn community_sort(value: Option<&str>) -> plamenu_db::group::TimelineSort {
    use plamenu_db::group::{TimelineSort, TopWindow};
    match value.unwrap_or("Hot") {
        "Active" => TimelineSort::Active,
        "Hot" => TimelineSort::Hot,
        "Old" => TimelineSort::Old,
        value if value.starts_with("Top") => TimelineSort::Top(match value {
            "TopDay" => TopWindow::Day,
            "TopWeek" => TopWindow::Week,
            "TopMonth" => TopWindow::Month,
            _ => TopWindow::All,
        }),
        _ => TimelineSort::New,
    }
}

/// Lemmy's post feed projected over Plamenu's already-policy-filtered public
/// and home timelines. Community Announce/boost rows are dereferenced and
/// deduplicated so clients receive the underlying post exactly once.
#[allow(clippy::too_many_lines)]
pub async fn list(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<GetPosts>,
) -> Result<Json<Value>, LemmyError> {
    let viewer = current.as_ref().map(|user| user.account.id);
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let cursor = query
        .page_cursor
        .as_deref()
        .and_then(|cursor| cursor.parse::<i64>().ok());
    let fetch_limit = limit.saturating_mul(page).saturating_mul(5).clamp(50, 500);
    let wanted_group = requested_community(&state, &query).await?;
    let group_offset = query
        .page_cursor
        .as_deref()
        .and_then(|cursor| cursor.strip_prefix("g:"))
        .and_then(|offset| offset.parse::<i64>().ok())
        .unwrap_or_else(|| (page - 1).saturating_mul(limit));
    let listing = query.type_.as_deref().unwrap_or("All");
    let timeline = if let Some(group) = &wanted_group {
        plamenu_db::group::attributed_timeline(
            &state.pool,
            group.id,
            viewer,
            community_sort(query.sort.as_deref()),
            limit,
            group_offset,
        )
        .await?
    } else {
        match listing {
            "Subscribed" | "ModeratorView" => {
                let user = current
                    .as_ref()
                    .ok_or_else(|| LemmyError::unauthorized("not_logged_in"))?;
                plamenu_db::status::home_timeline(
                    &state.pool,
                    user.account.id,
                    plamenu_db::user::TimelineOrder::Published,
                    cursor,
                    fetch_limit,
                )
                .await?
            }
            "Local" => {
                plamenu_db::status::public_timeline(
                    &state.pool,
                    true,
                    viewer,
                    false,
                    plamenu_db::user::TimelineOrder::Published,
                    cursor,
                    fetch_limit,
                )
                .await?
            }
            _ => {
                plamenu_db::status::public_timeline(
                    &state.pool,
                    false,
                    viewer,
                    false,
                    plamenu_db::user::TimelineOrder::Published,
                    cursor,
                    fetch_limit,
                )
                .await?
            }
        }
    };
    let group_rows_consumed = i64::try_from(timeline.len()).unwrap_or(i64::MAX);
    if (query.saved_only || query.liked_only || query.disliked_only) && current.is_none() {
        return Err(LemmyError::unauthorized("not_logged_in"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut candidate_entities = Vec::new();
    for timeline_status in timeline {
        let raw_cursor = timeline_status.id;
        let status = match timeline_status.reblog_of_id {
            Some(id) => match plamenu_db::status::find_by_id(&state.pool, id).await? {
                Some(status) => status,
                None => continue,
            },
            None => timeline_status,
        };
        if status.in_reply_to_id.is_some() || !seen.insert(status.id) {
            continue;
        }
        let group = if let Some(group) = &wanted_group {
            group.clone()
        } else {
            let Some(group) = crate::groups::communities_of_status(&state, &status)
                .await
                .map_err(LemmyError::from)?
                .into_iter()
                .next()
            else {
                continue;
            };
            group
        };
        candidate_entities.push((raw_cursor, status, group));
    }
    let mut candidates = Vec::new();
    for (raw_cursor, status, group) in candidate_entities {
        let view = post_view_with_read(&state, &status, &group, viewer, false).await?;
        if query.show_nsfw == Some(false) && view["post"]["nsfw"].as_bool() == Some(true) {
            continue;
        }
        if query.saved_only && view["saved"].as_bool() != Some(true) {
            continue;
        }
        if query.liked_only && view["my_vote"].as_i64() != Some(1) {
            continue;
        }
        if query.disliked_only && view["my_vote"].as_i64() != Some(-1) {
            continue;
        }
        candidates.push((raw_cursor, view));
    }
    let offset = if wanted_group.is_some() || cursor.is_some() {
        0
    } else {
        usize::try_from((page - 1).saturating_mul(limit)).unwrap_or(usize::MAX)
    };
    let mut page = candidates
        .into_iter()
        .skip(offset)
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    if let Some(account_id) = viewer {
        let status_ids = page
            .iter()
            .filter_map(|(_, view)| view["post"]["id"].as_i64())
            .collect::<Vec<_>>();
        let read_ids =
            plamenu_db::post_read::read_ids(&state.pool, account_id, &status_ids).await?;
        for (_, view) in &mut page {
            if let Some(id) = view["post"]["id"].as_i64() {
                view["read"] = Value::Bool(read_ids.contains(&id));
            }
        }
    }
    let next_page = if wanted_group.is_some() {
        (!page.is_empty())
            .then(|| format!("g:{}", group_offset.saturating_add(group_rows_consumed)))
    } else {
        page.last().map(|(cursor, _)| cursor.to_string())
    };
    Ok(Json(json!({
        "posts": page.into_iter().map(|(_, view)| view).collect::<Vec<_>>(),
        "next_page": next_page,
    })))
}

const MAX_MARK_READ_POSTS: usize = 100;

#[derive(Deserialize)]
pub struct MarkPostAsRead {
    post_ids: Vec<i32>,
    read: bool,
}

pub async fn mark_as_read(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<MarkPostAsRead>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    if form.post_ids.len() > MAX_MARK_READ_POSTS {
        return Err(LemmyError::bad_request("too_many_items"));
    }
    let native_ids =
        resolve_required_many(&state, Kind::Status, &form.post_ids, "couldnt_find_post").await?;
    let mut unique_ids = native_ids;
    unique_ids.sort_unstable();
    unique_ids.dedup();
    let statuses = plamenu_db::status::find_by_ids(&state.pool, &unique_ids).await?;
    if statuses.len() != unique_ids.len()
        || statuses
            .iter()
            .any(|status| status.in_reply_to_id.is_some() || status.reblog_of_id.is_some())
    {
        return Err(LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"));
    }
    plamenu_db::post_read::set_many(&state.pool, current.account.id, &unique_ids, form.read)
        .await?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct LikePost {
    post_id: i32,
    score: i16,
}

pub async fn like(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<LikePost>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let status = match form.score {
        1 => crate::actions::favourite_status(&state, &current.account, post_id).await,
        -1 => crate::actions::downvote_status(&state, &current.account, post_id).await,
        0 => {
            crate::actions::unfavourite_status(&state, &current.account, post_id).await?;
            crate::actions::undownvote_status(&state, &current.account, post_id).await
        }
        _ => return Err(LemmyError::bad_request("invalid_vote_value")),
    }
    .map_err(LemmyError::from)?;
    let group = community_of_status(&state, &status).await?;
    Ok(Json(json!({
        "post_view": post_view(&state, &status, &group, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct SavePost {
    post_id: i32,
    save: bool,
}

pub async fn save(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<SavePost>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let status = if form.save {
        crate::actions::bookmark_status(&state, &current.account, post_id).await
    } else {
        crate::actions::unbookmark_status(&state, &current.account, post_id).await
    }
    .map_err(LemmyError::from)?;
    let group = community_of_status(&state, &status).await?;
    Ok(Json(json!({
        "post_view": post_view(&state, &status, &group, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct DeletePost {
    post_id: i32,
    deleted: bool,
}

pub async fn delete(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<DeletePost>,
) -> Result<Json<Value>, LemmyError> {
    if !form.deleted {
        return Err(LemmyError::bad_request("couldnt_restore_post"));
    }
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let existing = plamenu_db::status::find_by_id(&state.pool, post_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
    let group = community_of_status(&state, &existing).await?;
    let deleted =
        crate::actions::delete_status(&state, &current.account, post_id, DeleteMode::Stub)
            .await
            .map_err(LemmyError::from)?;
    let mut view = post_view(&state, &deleted, &group, Some(current.account.id)).await?;
    view["post"]["deleted"] = Value::Bool(true);
    Ok(Json(json!({ "post_view": view })))
}
