use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::actions::{DeleteMode, EditParams, PostParams};
use crate::compose::PostFormat;

use super::auth::{LemmyUser, MaybeLemmyUser};
use super::entities::comment_view;
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

#[derive(Deserialize)]
pub struct CreateComment {
    content: String,
    post_id: i32,
    parent_id: Option<i32>,
}

pub async fn create(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreateComment>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let parent_id = resolve_required(
        &state,
        Kind::Status,
        form.parent_id.unwrap_or(form.post_id),
        "couldnt_find_post",
    )
    .await?;
    let parent = plamenu_db::status::find_by_id(&state.pool, parent_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
    let (stored, _) = crate::actions::post_status_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            text: &form.content,
            visibility: "public",
            in_reply_to_id: Some(parent.id),
            media_ids: &[],
            quoted_status_id: None,
            spoiler_text: "",
            sensitive: false,
            language: Some("en"),
            content_type: PostFormat::Markdown,
            poll: None,
            quote_approval_policy: None,
            group_id: None,
            title: None,
            external_url: None,
            event: None,
            kind: plamenu_ap::activity::PostKind::Note,
        },
        current.app_id,
    )
    .await
    .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "comment_view": comment_view(&state, &stored, Some(current.account.id)).await?,
        "recipient_ids": [],
    })))
}

#[derive(Deserialize)]
pub struct EditComment {
    comment_id: i32,
    content: Option<String>,
    language_id: Option<i32>,
}

pub async fn edit(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<EditComment>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    if !matches!(form.language_id, None | Some(0 | 1)) {
        return Err(LemmyError::bad_request("couldnt_find_language"));
    }
    let comment_id = resolve_required(
        &state,
        Kind::Status,
        form.comment_id,
        "couldnt_find_comment",
    )
    .await?;
    let stored = plamenu_db::status::find_by_id(&state.pool, comment_id)
        .await?
        .filter(|status| status.in_reply_to_id.is_some() && status.reblog_of_id.is_none())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_comment"))?;
    let edited = crate::actions::edit_status(
        &state,
        &current.account,
        stored.id,
        EditParams {
            text: form.content.as_deref(),
            content_type: form.content.as_ref().map(|_| PostFormat::Markdown),
            ..EditParams::default()
        },
    )
    .await
    .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "comment_view": comment_view(&state, &edited, Some(current.account.id)).await?,
        "recipient_ids": [],
    })))
}

#[derive(Deserialize)]
pub struct GetComment {
    id: i32,
}

pub async fn get(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<GetComment>,
) -> Result<Json<Value>, LemmyError> {
    let id = resolve_required(&state, Kind::Status, query.id, "couldnt_find_comment").await?;
    let comment = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .filter(|status| status.in_reply_to_id.is_some())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_comment"))?;
    Ok(Json(json!({
        "comment_view": comment_view(&state, &comment, current.as_ref().map(|user| user.account.id)).await?,
        "recipient_ids": [],
    })))
}

#[derive(Deserialize, Default)]
pub struct GetComments {
    post_id: Option<i32>,
    parent_id: Option<i32>,
    page: Option<i64>,
    limit: Option<i64>,
    sort: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<GetComments>,
) -> Result<Json<Value>, LemmyError> {
    let root_id = query
        .post_id
        .or(query.parent_id)
        .ok_or_else(|| LemmyError::bad_request("invalid_form"))?;
    let root_id = resolve_required(&state, Kind::Status, root_id, "couldnt_find_post").await?;
    let mut descendants = plamenu_db::status::descendants(&state.pool, root_id).await?;
    descendants.retain(|status| status.in_reply_to_id.is_some() && status.reblog_of_id.is_none());
    if query.sort.as_deref() == Some("Old") {
        descendants.sort_by_key(|status| status.id);
    } else if query.sort.as_deref() == Some("New") {
        descendants.sort_by_key(|status| std::cmp::Reverse(status.id));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 50);
    let offset = (query.page.unwrap_or(1).max(1) - 1).saturating_mul(limit);
    let mut comments = Vec::new();
    for status in descendants
        .iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        comments.push(
            comment_view(&state, status, current.as_ref().map(|user| user.account.id)).await?,
        );
    }
    Ok(Json(json!({ "comments": comments })))
}

#[derive(Deserialize)]
pub struct LikeComment {
    comment_id: i32,
    score: i16,
}

pub async fn like(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<LikeComment>,
) -> Result<Json<Value>, LemmyError> {
    let comment_id = resolve_required(
        &state,
        Kind::Status,
        form.comment_id,
        "couldnt_find_comment",
    )
    .await?;
    let status = match form.score {
        1 => crate::actions::favourite_status(&state, &current.account, comment_id).await,
        -1 => crate::actions::downvote_status(&state, &current.account, comment_id).await,
        0 => {
            crate::actions::unfavourite_status(&state, &current.account, comment_id).await?;
            crate::actions::undownvote_status(&state, &current.account, comment_id).await
        }
        _ => return Err(LemmyError::bad_request("invalid_vote_value")),
    }
    .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "comment_view": comment_view(&state, &status, Some(current.account.id)).await?,
        "recipient_ids": [],
    })))
}

#[derive(Deserialize)]
pub struct SaveComment {
    comment_id: i32,
    save: bool,
}

pub async fn save(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<SaveComment>,
) -> Result<Json<Value>, LemmyError> {
    let comment_id = resolve_required(
        &state,
        Kind::Status,
        form.comment_id,
        "couldnt_find_comment",
    )
    .await?;
    let status = if form.save {
        crate::actions::bookmark_status(&state, &current.account, comment_id).await
    } else {
        crate::actions::unbookmark_status(&state, &current.account, comment_id).await
    }
    .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "comment_view": comment_view(&state, &status, Some(current.account.id)).await?,
        "recipient_ids": [],
    })))
}

#[derive(Deserialize)]
pub struct DeleteComment {
    comment_id: i32,
    deleted: bool,
}

pub async fn delete(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<DeleteComment>,
) -> Result<Json<Value>, LemmyError> {
    if !form.deleted {
        return Err(LemmyError::bad_request("couldnt_restore_comment"));
    }
    let comment_id = resolve_required(
        &state,
        Kind::Status,
        form.comment_id,
        "couldnt_find_comment",
    )
    .await?;
    let deleted =
        crate::actions::delete_status(&state, &current.account, comment_id, DeleteMode::Stub)
            .await
            .map_err(LemmyError::from)?;
    let mut view = comment_view(&state, &deleted, Some(current.account.id)).await?;
    view["comment"]["deleted"] = Value::Bool(true);
    Ok(Json(json!({ "comment_view": view, "recipient_ids": [] })))
}
