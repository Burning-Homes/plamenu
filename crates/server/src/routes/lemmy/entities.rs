use plamenu_db::account::Account;
use serde_json::{Value, json};

use crate::AppState;
use crate::entities::{account_uri, avatar_url, header_url, rfc3339};

use super::error::LemmyError;

/// Lemmy's post and comment body fields contain Markdown, while Plamenu stores
/// the rendered, sanitized HTML for remote `ActivityPub` objects. Prefer the
/// original source for locally-authored rich text; otherwise recover Markdown
/// from the stored HTML so clients do not display tags as literal text.
fn lemmy_markdown_body(
    source: Option<plamenu_db::status::StatusSource>,
    rendered_html: &str,
) -> String {
    let source = source
        .filter(|source| !source.text.is_empty())
        .map(|source| source.text);
    lemmy_markdown_text(source.as_deref(), rendered_html)
}

/// Recover the Markdown-valued Lemmy field for content Plamenu stores as a
/// rendered HTML/source pair. Remote objects have no source, so they are
/// converted from their already-sanitized HTML.
pub(super) fn lemmy_markdown_text(source: Option<&str>, rendered_html: &str) -> String {
    source.filter(|source| !source.is_empty()).map_or_else(
        || {
            quick_html2md::html_to_markdown(rendered_html)
                .trim_end()
                .to_owned()
        },
        str::to_owned,
    )
}

/// Lemmy requires an instance id even though Plamenu deliberately does not
/// normalize domains into an `instances` table. A bounded stable hash preserves
/// equality semantics without exposing a fabricated database row id.
pub fn instance_id_for_domain(domain: Option<&str>) -> i64 {
    let Some(domain) = domain else {
        return 1;
    };
    let hash = domain.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    i64::from(hash & 0x7fff_ffff) + 2
}

pub async fn community(
    state: &AppState,
    account: &Account,
    viewer: Option<i64>,
) -> Result<Value, LemmyError> {
    if !account.is_group() {
        return Err(LemmyError::new(
            axum::http::StatusCode::NOT_FOUND,
            "couldnt_find_community",
        ));
    }
    let sidecar = plamenu_db::group::find(&state.pool, account.id).await?;
    let published = rfc3339(account.created_at).map_err(LemmyError::from)?;
    let aggregate = plamenu_db::group::community_aggregate(&state.pool, account.id).await?;
    let follow = match viewer {
        Some(viewer) => plamenu_db::follow::find(&state.pool, viewer, account.id).await?,
        None => None,
    };
    let subscribed = match follow {
        Some(edge) if edge.pending => "Pending",
        Some(_) => "Subscribed",
        None => "NotSubscribed",
    };
    let blocked = match viewer {
        Some(viewer) => plamenu_db::block::exists(&state.pool, viewer, account.id).await?,
        None => false,
    };
    let banned = match viewer {
        Some(viewer) => {
            plamenu_db::group::affiliation_of(&state.pool, account.id, viewer).await?
                == Some(plamenu_db::group::Affiliation::Outcast)
        }
        None => false,
    };
    let description = lemmy_markdown_text(
        (!account.note_source.is_empty()).then_some(account.note_source.as_str()),
        &account.note,
    );
    let community = json!({
        "id": account.id,
        "name": account.username,
        "title": if account.display_name.is_empty() { account.username.as_str() } else { account.display_name.as_str() },
        "description": (!description.is_empty()).then_some(description),
        "removed": account.suspended(),
        "published": published,
        "updated": rfc3339(account.updated_at).map_err(LemmyError::from)?,
        "deleted": false,
        "nsfw": sidecar.as_ref().is_some_and(|group| group.sensitive),
        "actor_id": account_uri(&state.config.domain, account),
        "local": account.is_local(),
        "icon": avatar_url(&state.config.domain, account, false),
        "banner": header_url(&state.config.domain, account, false),
        "hidden": !account.discoverable.unwrap_or(false),
        "posting_restricted_to_mods": sidecar.as_ref().is_some_and(plamenu_db::group::Group::posting_restricted_to_mods),
        "instance_id": instance_id_for_domain(account.domain.as_deref()),
        "visibility": "Public",
    });
    Ok(json!({
        "community": community,
        "subscribed": subscribed,
        "blocked": blocked,
        "counts": {
            "community_id": account.id,
            "subscribers": aggregate.subscribers,
            "posts": aggregate.posts,
            "comments": aggregate.comments,
            "published": published,
            "users_active_day": aggregate.active_day,
            "users_active_week": aggregate.active_week,
            "users_active_month": aggregate.active_month,
            "users_active_half_year": aggregate.active_half_year,
            "subscribers_local": aggregate.subscribers_local,
        },
        "banned_from_community": banned,
    }))
}

pub async fn post_view(
    state: &AppState,
    status: &plamenu_db::status::Status,
    community_account: &Account,
    viewer: Option<i64>,
) -> Result<Value, LemmyError> {
    let read = match viewer {
        Some(account_id) => {
            plamenu_db::post_read::contains(&state.pool, account_id, status.id).await?
        }
        None => false,
    };
    post_view_with_read(state, status, community_account, viewer, read).await
}

/// Build a post view when a page handler has already loaded read state for the
/// whole page in one query.
pub async fn post_view_with_read(
    state: &AppState,
    status: &plamenu_db::status::Status,
    community_account: &Account,
    viewer: Option<i64>,
    read: bool,
) -> Result<Value, LemmyError> {
    let creator = plamenu_db::account::find_by_id(&state.pool, status.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(axum::http::StatusCode::NOT_FOUND, "person_not_found"))?;
    let rendered =
        crate::entities::render_status(&state.pool, &state.config.domain, status, viewer)
            .await
            .map_err(LemmyError::from)?;
    let source = plamenu_db::status::source_of(&state.pool, status.id).await?;
    let body = lemmy_markdown_body(source, &status.content);
    let engagement = plamenu_db::status::engagement_for(&state.pool, &[status.id])
        .await?
        .remove(&status.id)
        .unwrap_or_default();
    let locked =
        plamenu_db::group::thread_locked(&state.pool, community_account.id, status.id).await?;
    let featured = !plamenu_db::pin::pinned_of(&state.pool, community_account.id, &[status.id])
        .await?
        .is_empty();
    let creator_admin = if creator.is_local() {
        plamenu_db::role::for_account(&state.pool, creator.id)
            .await?
            .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR))
    } else {
        false
    };
    let creator_mod = matches!(
        plamenu_db::group::affiliation_of(&state.pool, community_account.id, creator.id).await?,
        Some(plamenu_db::group::Affiliation::Owner | plamenu_db::group::Affiliation::Moderator)
    );
    let creator_banned =
        plamenu_db::group::affiliation_of(&state.pool, community_account.id, creator.id).await?
            == Some(plamenu_db::group::Affiliation::Outcast);
    let community_view = community(state, community_account, viewer).await?;
    let published = rfc3339(status.created_at).map_err(LemmyError::from)?;
    let my_vote = if rendered["favourited"].as_bool() == Some(true) {
        Some(1)
    } else if rendered["downvoted"].as_bool() == Some(true) {
        Some(-1)
    } else {
        None
    };
    Ok(json!({
        "post": {
            "id": status.id,
            "name": status.title.as_deref().filter(|title| !title.is_empty()).unwrap_or("Untitled post"),
            "url": status.external_url,
            "body": body,
            "creator_id": creator.id,
            "community_id": community_account.id,
            "removed": false,
            "locked": locked,
            "published": published,
            "updated": status.edited_at.and_then(|at| rfc3339(at).ok()),
            "deleted": false,
            "nsfw": status.sensitive,
            "ap_id": crate::entities::status_uri_for_account(&state.config.domain, status, &creator),
            "local": status.uri.is_none(),
            "language_id": 1,
            "featured_community": featured,
            "featured_local": false,
        },
        "creator": person(state, &creator, creator_admin).await?["person"],
        "community": community_view["community"],
        "creator_banned_from_community": creator_banned,
        "banned_from_community": community_view["banned_from_community"],
        "creator_is_moderator": creator_mod,
        "creator_is_admin": creator_admin,
        "counts": {
            "post_id": status.id,
            "comments": engagement.replies,
            "score": engagement.favourites - engagement.dislikes,
            "upvotes": engagement.favourites,
            "downvotes": engagement.dislikes,
            "published": published,
            "newest_comment_time": published,
        },
        "subscribed": community_view["subscribed"],
        "saved": rendered["bookmarked"],
        "read": read,
        "hidden": false,
        "creator_blocked": false,
        "my_vote": my_vote,
        "unread_comments": 0,
    }))
}

async fn root_post(
    state: &AppState,
    comment: &plamenu_db::status::Status,
) -> Result<plamenu_db::status::Status, LemmyError> {
    let mut current = comment.clone();
    for _ in 0..40 {
        let Some(parent_id) = current.in_reply_to_id else {
            return Ok(current);
        };
        current = plamenu_db::status::find_by_id(&state.pool, parent_id)
            .await?
            .ok_or_else(|| {
                LemmyError::new(axum::http::StatusCode::NOT_FOUND, "couldnt_find_post")
            })?;
    }
    Err(LemmyError::bad_request("max_comment_depth_reached"))
}

pub async fn comment_view(
    state: &AppState,
    comment: &plamenu_db::status::Status,
    viewer: Option<i64>,
) -> Result<Value, LemmyError> {
    if comment.in_reply_to_id.is_none() || comment.reblog_of_id.is_some() {
        return Err(LemmyError::new(
            axum::http::StatusCode::NOT_FOUND,
            "couldnt_find_comment",
        ));
    }
    let root = root_post(state, comment).await?;
    let group = crate::groups::communities_of_status(state, &root)
        .await
        .map_err(LemmyError::from)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            LemmyError::new(axum::http::StatusCode::NOT_FOUND, "couldnt_find_community")
        })?;
    let creator = plamenu_db::account::find_by_id(&state.pool, comment.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(axum::http::StatusCode::NOT_FOUND, "person_not_found"))?;
    let rendered =
        crate::entities::render_status(&state.pool, &state.config.domain, comment, viewer)
            .await
            .map_err(LemmyError::from)?;
    let source = plamenu_db::status::source_of(&state.pool, comment.id).await?;
    let content = lemmy_markdown_body(source, &comment.content);
    let engagement = plamenu_db::status::engagement_for(&state.pool, &[comment.id])
        .await?
        .remove(&comment.id)
        .unwrap_or_default();
    let ancestors = plamenu_db::status::ancestors(&state.pool, comment.id).await?;
    let path = std::iter::once(root.id)
        .chain(
            ancestors
                .iter()
                .filter(|status| status.in_reply_to_id.is_some())
                .map(|status| status.id),
        )
        .chain(std::iter::once(comment.id))
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(".");
    let creator_admin = creator.is_local()
        && plamenu_db::role::for_account(&state.pool, creator.id)
            .await?
            .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
    let creator_mod = matches!(
        plamenu_db::group::affiliation_of(&state.pool, group.id, creator.id).await?,
        Some(plamenu_db::group::Affiliation::Owner | plamenu_db::group::Affiliation::Moderator)
    );
    let group_view = community(state, &group, viewer).await?;
    let published = rfc3339(comment.created_at).map_err(LemmyError::from)?;
    let my_vote = if rendered["favourited"].as_bool() == Some(true) {
        Some(1)
    } else if rendered["downvoted"].as_bool() == Some(true) {
        Some(-1)
    } else {
        None
    };
    Ok(json!({
        "comment": {
            "id": comment.id,
            "creator_id": creator.id,
            "post_id": root.id,
            "content": content,
            "removed": false,
            "published": published,
            "updated": comment.edited_at.and_then(|at| rfc3339(at).ok()),
            "deleted": false,
            "ap_id": crate::entities::status_uri_for_account(&state.config.domain, comment, &creator),
            "local": comment.uri.is_none(),
            "path": format!("0.{path}"),
            "distinguished": false,
            "language_id": 1,
        },
        "creator": person(state, &creator, creator_admin).await?["person"],
        // A comment embeds only the post entity, not the viewer's post-view
        // read flag, so avoid an otherwise per-comment post-read lookup.
        "post": post_view_with_read(state, &root, &group, viewer, false).await?["post"],
        "community": group_view["community"],
        "counts": {
            "comment_id": comment.id,
            "score": engagement.favourites - engagement.dislikes,
            "upvotes": engagement.favourites,
            "downvotes": engagement.dislikes,
            "published": published,
            "child_count": engagement.replies,
        },
        "creator_banned_from_community": plamenu_db::group::affiliation_of(&state.pool, group.id, creator.id).await? == Some(plamenu_db::group::Affiliation::Outcast),
        "banned_from_community": group_view["banned_from_community"],
        "creator_is_moderator": creator_mod,
        "creator_is_admin": creator_admin,
        "subscribed": group_view["subscribed"],
        "saved": rendered["bookmarked"],
        "creator_blocked": false,
        "my_vote": my_vote,
    }))
}

pub async fn person_counts(state: &AppState, account_id: i64) -> Result<Value, LemmyError> {
    let (posts, comments) =
        plamenu_db::status::count_posts_comments_by_account(&state.pool, account_id).await?;
    Ok(json!({
        "person_id": account_id,
        "post_count": posts,
        "comment_count": comments,
    }))
}

pub async fn person(
    state: &AppState,
    account: &Account,
    is_admin: bool,
) -> Result<Value, LemmyError> {
    let published = rfc3339(account.created_at).map_err(LemmyError::from)?;
    let updated = rfc3339(account.updated_at).map_err(LemmyError::from)?;
    let fields = account
        .fields
        .as_array()
        .map(|fields| {
            fields
                .iter()
                .enumerate()
                .filter_map(|(index, field)| {
                    Some(json!({
                        "id": i64::try_from(index).ok()?,
                        "label": field.get("name")?.as_str()?,
                        "text": field.get("value")?.as_str()?,
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let person = json!({
        "id": account.id,
        "name": account.username,
        "display_name": (!account.display_name.is_empty()).then_some(account.display_name.as_str()),
        "avatar": avatar_url(&state.config.domain, account, false),
        "banned": account.suspended(),
        "published": published,
        "updated": updated,
        "actor_id": account_uri(&state.config.domain, account),
        // Lemmy stores/renderers expect the editable Markdown source here,
        // not Plamenu's composed HTML actor note.
        "bio": (!account.note_source.is_empty()).then_some(account.note_source.as_str()),
        "local": account.is_local(),
        "banner": header_url(&state.config.domain, account, false),
        "deleted": false,
        "bot_account": account.is_bot,
        "instance_id": instance_id_for_domain(account.domain.as_deref()),
        "extra_fields": fields,
    });
    Ok(json!({
        "person": person,
        "counts": person_counts(state, account.id).await?,
        "is_admin": is_admin,
    }))
}

pub async fn local_user_view(
    state: &AppState,
    current: &crate::auth::CurrentUser,
) -> Result<Value, LemmyError> {
    let settings = plamenu_db::user::settings_by_user_id(&state.pool, current.user.id)
        .await?
        .unwrap_or_default();
    let is_admin = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await?
        .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
    let person_view = person(state, &current.account, is_admin).await?;
    let locale = plamenu_db::user::locale(&state.pool, current.user.id).await?;
    let mut local_user = json!({
            "id": current.user.id,
            "person_id": current.account.id,
            "email": current.user.email,
            "show_nsfw": true,
            "default_sort_type": "Active",
            "default_listing_type": "All",
            "interface_language": locale.unwrap_or(settings.posting_default_language),
            "show_avatars": true,
            "show_scores": true,
            "show_bot_accounts": true,
            "show_read_posts": true,
            "email_verified": current.user.confirmed(),
            "accepted_application": current.user.approved,
            "totp_2fa_enabled": current.user.otp_required_for_login,
    });
    if let Some(object) = local_user.as_object_mut() {
        for (key, value) in
            plamenu_db::lemmy_user::preferences(&state.pool, current.user.id).await?
        {
            object.insert(key, value);
        }
        // Identity/security fields can never be overridden by compatibility
        // preferences, including rows written by an older adapter.
        object.insert("id".into(), Value::from(current.user.id));
        object.insert("person_id".into(), Value::from(current.account.id));
        object.insert("email".into(), json!(current.user.email));
        object.insert(
            "email_verified".into(),
            Value::Bool(current.user.confirmed()),
        );
        object.insert(
            "accepted_application".into(),
            Value::Bool(current.user.approved),
        );
        object.insert(
            "totp_2fa_enabled".into(),
            Value::Bool(current.user.otp_required_for_login),
        );
    }
    Ok(json!({
        "local_user": local_user,
        "local_user_vote_display_mode": {
            "local_user_id": current.user.id,
            "score": true,
            "upvotes": true,
            "downvotes": true,
            "upvote_percentage": true,
        },
        "person": person_view["person"],
        "counts": person_view["counts"],
    }))
}

#[cfg(test)]
mod tests {
    use super::lemmy_markdown_body;

    #[test]
    fn remote_html_becomes_lemmy_markdown() {
        let html = concat!(
            "<p><span class=\"h-card\"><a href=\"https://example.test/@alice\" ",
            "class=\"u-url mention\" rel=\"nofollow noopener noreferrer\">",
            "@<span>alice</span></a></span> ping!</p>",
            "<blockquote><p><strong>Useful</strong> context</p></blockquote>",
        );

        assert_eq!(
            lemmy_markdown_body(None, html),
            "[@alice](https://example.test/@alice) ping!\n\n> **Useful** context"
        );
    }

    #[test]
    fn stored_source_wins_without_rewriting() {
        let source = plamenu_db::status::StatusSource {
            text: "original *Markdown*".to_owned(),
            content_type: "text/markdown".to_owned(),
        };
        assert_eq!(
            lemmy_markdown_body(Some(source), "<p>rendered HTML</p>"),
            "original *Markdown*"
        );
    }
}
