use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::LemmyUser;
use super::entities::{comment_view, person};
use super::error::LemmyError;
use super::ids::resolve_required;
use crate::actions::{DeleteMode, EditParams, PostParams};
use crate::compose::PostFormat;
use plamenu_db::lemmy_id::Kind;
use plamenu_db::notification::{Notification, NotificationFilter};

#[derive(Default, Deserialize)]
pub struct InboxQuery {
    page: Option<i64>,
    limit: Option<i64>,
    #[serde(default)]
    unread_only: bool,
}

async fn mention_page(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    query: &InboxQuery,
    replies: bool,
) -> Result<Vec<Value>, LemmyError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let fetch = (page * limit * 3).clamp(50, 500);
    let kinds = vec!["mention".to_owned()];
    let notifications = plamenu_db::notification::list(
        &state.pool,
        current.account.id,
        None,
        None,
        None,
        NotificationFilter {
            kinds: Some(&kinds),
            ..NotificationFilter::default()
        },
        fetch,
    )
    .await?;
    let ids = notifications.iter().map(|item| item.id).collect::<Vec<_>>();
    let read = plamenu_db::lemmy_inbox::read_states(
        &state.pool,
        current.user.id,
        current.account.id,
        &ids,
    )
    .await?;
    let mut selected = Vec::new();
    for notification in notifications {
        let Some(status_id) = notification.status_id else {
            continue;
        };
        let Some(status) = plamenu_db::status::find_by_id(&state.pool, status_id).await? else {
            continue;
        };
        if status.in_reply_to_id.is_none() || status.reblog_of_id.is_some() {
            continue;
        }
        let is_reply = match status.in_reply_to_id {
            Some(parent_id) => plamenu_db::status::find_by_id(&state.pool, parent_id)
                .await?
                .is_some_and(|parent| parent.account_id == current.account.id),
            None => false,
        };
        if is_reply != replies {
            continue;
        }
        let item_read = read.get(&notification.id).copied().unwrap_or(false);
        if query.unread_only && item_read {
            continue;
        }
        match mention_view(state, current, &notification, &status, item_read, replies).await {
            Ok(view) => selected.push(view),
            // Native notification rows can legitimately outlive content or a
            // group that was purged later. One stale item must not make the
            // entire Lemmy inbox unusable.
            Err(error) if error.status == StatusCode::NOT_FOUND => {}
            Err(error) => return Err(error),
        }
    }
    let offset = (page - 1).saturating_mul(limit);
    Ok(selected
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect())
}

async fn mention_view(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    notification: &Notification,
    status: &plamenu_db::status::Status,
    read: bool,
    reply: bool,
) -> Result<Value, LemmyError> {
    let base = comment_view(state, status, Some(current.account.id)).await?;
    let mut object = base.as_object().cloned().unwrap_or_default();
    let is_admin = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
    object.insert(
        "recipient".into(),
        person(state, &current.account, is_admin).await?["person"].clone(),
    );
    let kind = if reply {
        "comment_reply"
    } else {
        "person_mention"
    };
    object.insert(
        kind.into(),
        json!({
            "id": notification.id,
            "recipient_id": current.account.id,
            "comment_id": status.id,
            "read": read,
            "published": crate::entities::rfc3339(notification.created_at).map_err(LemmyError::from)?,
        }),
    );
    Ok(Value::Object(object))
}

pub async fn replies(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<InboxQuery>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("read").map_err(LemmyError::from)?;
    Ok(Json(
        json!({ "replies": mention_page(&state, &current, &query, true).await? }),
    ))
}

pub async fn mentions(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<InboxQuery>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("read").map_err(LemmyError::from)?;
    Ok(Json(
        json!({ "mentions": mention_page(&state, &current, &query, false).await? }),
    ))
}

#[derive(Deserialize)]
pub struct MarkReplyRead {
    comment_reply_id: i32,
    read: bool,
}

#[derive(Deserialize)]
pub struct MarkMentionRead {
    person_mention_id: i32,
    read: bool,
}

async fn mark_notification(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    alias: i32,
    read: bool,
    reply: bool,
) -> Result<Value, LemmyError> {
    let id = resolve_required(
        state,
        Kind::Notification,
        alias,
        "couldnt_find_comment_reply",
    )
    .await?;
    let notification =
        plamenu_db::lemmy_inbox::find_notification(&state.pool, current.account.id, id)
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_comment_reply"))?;
    let status = plamenu_db::status::find_by_id(
        &state.pool,
        notification
            .status_id
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_comment"))?,
    )
    .await?
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_comment"))?;
    let is_reply = match status.in_reply_to_id {
        Some(parent_id) => plamenu_db::status::find_by_id(&state.pool, parent_id)
            .await?
            .is_some_and(|parent| parent.account_id == current.account.id),
        None => false,
    };
    if is_reply != reply {
        return Err(LemmyError::new(
            StatusCode::NOT_FOUND,
            "couldnt_find_comment_reply",
        ));
    }
    plamenu_db::lemmy_inbox::set_read(&state.pool, current.account.id, id, read).await?;
    mention_view(state, current, &notification, &status, read, reply).await
}

pub async fn mark_reply_read(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<MarkReplyRead>,
) -> Result<Json<Value>, LemmyError> {
    let view = mark_notification(&state, &current, form.comment_reply_id, form.read, true).await?;
    Ok(Json(json!({ "comment_reply_view": view })))
}

pub async fn mark_mention_read(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<MarkMentionRead>,
) -> Result<Json<Value>, LemmyError> {
    let view =
        mark_notification(&state, &current, form.person_mention_id, form.read, false).await?;
    Ok(Json(json!({ "person_mention_view": view })))
}

pub async fn mark_all_read(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
) -> Result<Json<Value>, LemmyError> {
    plamenu_db::lemmy_inbox::mark_all_read(&state.pool, current.user.id, current.account.id)
        .await?;
    Ok(Json(json!({ "replies": [] })))
}

#[derive(Default, Deserialize)]
pub struct PrivateMessageQuery {
    page: Option<i64>,
    limit: Option<i64>,
    #[serde(default)]
    unread_only: bool,
    creator_id: Option<i32>,
}

fn mention_handle(account: &plamenu_db::account::Account) -> String {
    account.domain.as_ref().map_or_else(
        || format!("@{}", account.username),
        |domain| format!("@{}@{domain}", account.username),
    )
}

async fn private_message_view(
    state: &AppState,
    viewer: &crate::auth::CurrentUser,
    status: &plamenu_db::status::Status,
    row: &plamenu_db::conversation::AccountConversation,
) -> Result<Value, LemmyError> {
    let creator = plamenu_db::account::find_by_id(&state.pool, status.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_person"))?;
    let recipient_id = if creator.id == viewer.account.id {
        row.participant_account_ids.first().copied()
    } else {
        Some(viewer.account.id)
    }
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_person"))?;
    let recipient = plamenu_db::account::find_by_id(&state.pool, recipient_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_person"))?;
    let source = plamenu_db::status::source_of(&state.pool, status.id)
        .await?
        .map_or_else(|| status.content.clone(), |source| source.text);
    let content = source
        .split_once(char::is_whitespace)
        .filter(|(first, _)| first.starts_with('@'))
        .map_or(source.as_str(), |(_, rest)| rest)
        .to_owned();
    Ok(json!({
        "private_message": {
            "id": status.id,
            "creator_id": creator.id,
            "recipient_id": recipient.id,
            "content": content,
            "deleted": false,
            "read": !row.unread,
            "published": crate::entities::rfc3339(status.created_at).map_err(LemmyError::from)?,
            "updated": status.edited_at.and_then(|at| crate::entities::rfc3339(at).ok()),
            "ap_id": crate::entities::status_uri_for_account(&state.config.domain, status, &creator),
            "local": status.uri.is_none(),
        },
        "creator": person(state, &creator, false).await?["person"],
        "recipient": person(state, &recipient, false).await?["person"],
    }))
}

pub async fn private_messages(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<PrivateMessageQuery>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("read").map_err(LemmyError::from)?;
    let creator = match query.creator_id {
        Some(id) => Some(resolve_required(&state, Kind::Account, id, "couldnt_find_person").await?),
        None => None,
    };
    let rows =
        plamenu_db::conversation::list(&state.pool, current.account.id, None, None, None, 500)
            .await?;
    let mut messages = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        if query.unread_only && !row.unread {
            continue;
        }
        for id in row.status_ids.iter().rev() {
            if !seen.insert(*id) {
                continue;
            }
            let Some(status) = plamenu_db::status::find_by_id(&state.pool, *id).await? else {
                continue;
            };
            if status.visibility != "direct" || creator.is_some_and(|id| id != status.account_id) {
                continue;
            }
            messages.push((status, row.clone()));
        }
    }
    messages.sort_by_key(|(status, _)| std::cmp::Reverse(status.id));
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let offset = (query.page.unwrap_or(1).max(1) - 1).saturating_mul(limit);
    let mut views = Vec::new();
    for (status, row) in messages
        .iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        views.push(private_message_view(&state, &current, status, row).await?);
    }
    Ok(Json(json!({ "private_messages": views })))
}

#[derive(Deserialize)]
pub struct CreatePrivateMessage {
    content: String,
    recipient_id: i32,
}

pub async fn create_private_message(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreatePrivateMessage>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let recipient_id = resolve_required(
        &state,
        Kind::Account,
        form.recipient_id,
        "couldnt_find_person",
    )
    .await?;
    let recipient = plamenu_db::account::find_by_id(&state.pool, recipient_id)
        .await?
        .filter(|account| !account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_person"))?;
    let text = format!("{} {}", mention_handle(&recipient), form.content);
    let (status, _) = crate::actions::post_status_for_application(
        &state,
        PostParams {
            username: &current.account.username,
            text: &text,
            visibility: "direct",
            in_reply_to_id: None,
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
    let row = plamenu_db::lemmy_inbox::conversation_for_status(
        &state.pool,
        current.account.id,
        status.id,
    )
    .await?
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    Ok(Json(json!({
        "private_message_view": private_message_view(&state, &current, &status, &row).await?,
    })))
}

#[derive(Deserialize)]
pub struct EditPrivateMessage {
    private_message_id: i32,
    content: String,
}

pub async fn edit_private_message(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<EditPrivateMessage>,
) -> Result<Json<Value>, LemmyError> {
    let id = resolve_required(
        &state,
        Kind::PrivateMessage,
        form.private_message_id,
        "couldnt_find_private_message",
    )
    .await?;
    let stored = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .filter(|status| status.visibility == "direct")
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    let recipients = plamenu_db::mention::for_statuses(&state.pool, &[id], false)
        .await?
        .remove(&id)
        .unwrap_or_default();
    let prefix = recipients
        .iter()
        .filter(|account| account.id != current.account.id)
        .map(mention_handle)
        .collect::<Vec<_>>()
        .join(" ");
    let text = if prefix.is_empty() {
        form.content
    } else {
        format!("{prefix} {}", form.content)
    };
    let edited = crate::actions::edit_status(
        &state,
        &current.account,
        stored.id,
        EditParams {
            text: Some(&text),
            content_type: Some(PostFormat::Markdown),
            ..EditParams::default()
        },
    )
    .await
    .map_err(LemmyError::from)?;
    let row = plamenu_db::lemmy_inbox::conversation_for_status(&state.pool, current.account.id, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    Ok(Json(
        json!({ "private_message_view": private_message_view(&state, &current, &edited, &row).await? }),
    ))
}

#[derive(Deserialize)]
pub struct DeletePrivateMessage {
    private_message_id: i32,
    deleted: bool,
}

pub async fn delete_private_message(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<DeletePrivateMessage>,
) -> Result<Json<Value>, LemmyError> {
    if !form.deleted {
        return Err(LemmyError::bad_request("couldnt_restore_private_message"));
    }
    let id = resolve_required(
        &state,
        Kind::PrivateMessage,
        form.private_message_id,
        "couldnt_find_private_message",
    )
    .await?;
    let row = plamenu_db::lemmy_inbox::conversation_for_status(&state.pool, current.account.id, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    let deleted = crate::actions::delete_status(&state, &current.account, id, DeleteMode::Stub)
        .await
        .map_err(LemmyError::from)?;
    let mut view = private_message_view(&state, &current, &deleted, &row).await?;
    view["private_message"]["deleted"] = Value::Bool(true);
    Ok(Json(json!({ "private_message_view": view })))
}

#[derive(Deserialize)]
pub struct MarkPrivateMessageRead {
    private_message_id: i32,
    read: bool,
}

pub async fn mark_private_message_read(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<MarkPrivateMessageRead>,
) -> Result<Json<Value>, LemmyError> {
    let id = resolve_required(
        &state,
        Kind::PrivateMessage,
        form.private_message_id,
        "couldnt_find_private_message",
    )
    .await?;
    let row = plamenu_db::lemmy_inbox::conversation_for_status(&state.pool, current.account.id, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    let row =
        plamenu_db::conversation::set_unread(&state.pool, current.account.id, row.id, !form.read)
            .await?
            .ok_or_else(|| {
                LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message")
            })?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_private_message"))?;
    Ok(Json(
        json!({ "private_message_view": private_message_view(&state, &current, &status, &row).await? }),
    ))
}
