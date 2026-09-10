use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::{LemmyUser, MaybeLemmyUser};
use super::entities::{community, person};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::group::{Affiliation, MembershipPolicy, PostingPolicy};
use plamenu_db::lemmy_id::Kind;

#[derive(Deserialize, Default)]
pub struct GetCommunity {
    id: Option<i32>,
    name: Option<String>,
}

async fn resolve(
    state: &AppState,
    query: &GetCommunity,
) -> Result<plamenu_db::account::Account, LemmyError> {
    if let Some(id) = query.id {
        let id = resolve_required(state, Kind::Account, id, "couldnt_find_community").await?;
        return plamenu_db::account::find_by_id(&state.pool, id)
            .await?
            .filter(plamenu_db::account::Account::is_group)
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"));
    }
    let name = query
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LemmyError::bad_request("invalid_form"))?;
    let name = name.strip_prefix('!').unwrap_or(name);
    if let Some((username, domain)) = name.rsplit_once('@') {
        if state.config.is_local_domain(domain) {
            plamenu_db::account::find_local_by_username(&state.pool, username)
                .await?
                .filter(plamenu_db::account::Account::is_group)
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
        } else {
            plamenu_db::account::find_remote_group_by_acct(&state.pool, username, domain)
                .await?
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
        }
    } else {
        plamenu_db::account::find_local_by_username(&state.pool, name)
            .await?
            .filter(plamenu_db::account::Account::is_group)
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
    }
}

async fn moderators(
    state: &AppState,
    group: &plamenu_db::account::Account,
) -> Result<Vec<Value>, LemmyError> {
    let entries = plamenu_db::group::elevated(&state.pool, group.id).await?;
    let accounts = plamenu_db::account::find_by_ids(
        &state.pool,
        &entries
            .iter()
            .map(|entry| entry.account_id)
            .collect::<Vec<_>>(),
    )
    .await?;
    let entity = community(state, group, None).await?["community"].clone();
    let mut views = Vec::new();
    for account in &accounts {
        views.push(json!({
            "community": entity,
            "moderator": person(state, account, false).await?["person"],
        }));
    }
    Ok(views)
}

pub async fn get(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<GetCommunity>,
) -> Result<Json<Value>, LemmyError> {
    let group = resolve(&state, &query).await?;
    Ok(Json(json!({
        "community_view": community(&state, &group, current.as_ref().map(|user| user.account.id)).await?,
        "moderators": moderators(&state, &group).await?,
        "discussion_languages": [1],
    })))
}

#[derive(Deserialize)]
pub struct CreateCommunity {
    name: String,
    title: String,
    description: Option<String>,
    icon: Option<String>,
    banner: Option<String>,
    #[serde(default)]
    nsfw: bool,
    #[serde(default)]
    posting_restricted_to_mods: bool,
    discussion_languages: Option<Vec<i32>>,
    visibility: Option<String>,
}

fn validate_community_shape(
    _icon: Option<&str>,
    _banner: Option<&str>,
    languages: Option<&[i32]>,
    visibility: Option<&str>,
) -> Result<(), LemmyError> {
    if languages.is_some_and(|ids| ids.iter().any(|id| !matches!(id, 0 | 1))) {
        return Err(LemmyError::bad_request("couldnt_find_language"));
    }
    if visibility.is_some_and(|value| value != "Public") {
        return Err(LemmyError::bad_request("unsupported_community_visibility"));
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreateCommunity>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    validate_community_shape(
        form.icon.as_deref(),
        form.banner.as_deref(),
        form.discussion_languages.as_deref(),
        form.visibility.as_deref(),
    )?;
    let avatar = match form.icon.as_deref().filter(|url| !url.is_empty()) {
        Some(url) => Some(
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_community_image_url"))?,
        ),
        None => None,
    };
    let banner = match form.banner.as_deref().filter(|url| !url.is_empty()) {
        Some(url) => Some(
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_community_image_url"))?,
        ),
        None => None,
    };
    let settings = state.settings_cache.get(&state.pool).await?;
    let is_staff = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await?
        .is_some_and(|role| role.privileged());
    if !crate::groups::may_create(settings.group_creation_policy(), is_staff) {
        return Err(LemmyError::new(
            StatusCode::FORBIDDEN,
            "community_creation_admin_only",
        ));
    }
    let (mut account, _) = crate::groups::create_group(
        &state,
        crate::groups::CreateGroupParams {
            name: &form.name,
            display_name: &form.title,
            membership_policy: MembershipPolicy::Open,
            posting_policy: if form.posting_restricted_to_mods {
                PostingPolicy::Mods
            } else {
                PostingPolicy::Anyone
            },
            created_by: current.account.id,
            enforce_username_blocklist: true,
            enforce_account_quota: true,
        },
    )
    .await
    .map_err(crate::error::ApiError::from)
    .map_err(LemmyError::from)?;
    let description = form.description.unwrap_or_default();
    if !description.is_empty()
        || form.nsfw
        || form.visibility.as_deref() == Some("LocalOnly")
        || avatar.is_some()
        || banner.is_some()
    {
        let composed =
            crate::compose::compose(&state, &description, crate::compose::PostFormat::Markdown)
                .await
                .map_err(LemmyError::from)?;
        crate::groups::update_settings(
            &state,
            &account,
            crate::groups::GroupSettings {
                display_name: &form.title,
                note_html: &composed.html,
                note_source: &description,
                policy: MembershipPolicy::Open,
                sensitive: form.nsfw,
                posting_policy: if form.posting_restricted_to_mods {
                    PostingPolicy::Mods
                } else {
                    PostingPolicy::Anyone
                },
                discoverable: true,
                profile: crate::groups::GroupProfileEdit {
                    avatar,
                    header: banner,
                    ..crate::groups::GroupProfileEdit::default()
                },
            },
        )
        .await
        .map_err(LemmyError::from)?;
        account = plamenu_db::account::find_by_id(&state.pool, account.id)
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    }
    Ok(Json(json!({
        "community_view": community(&state, &account, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct EditCommunity {
    community_id: i32,
    title: Option<String>,
    description: Option<String>,
    icon: Option<String>,
    banner: Option<String>,
    nsfw: Option<bool>,
    posting_restricted_to_mods: Option<bool>,
    discussion_languages: Option<Vec<i32>>,
    visibility: Option<String>,
}

// This stays as one transaction-shaped flow: resolve and authorize the group,
// consume optional uploads, update native settings, then clear requested images.
#[allow(clippy::too_many_lines)]
pub async fn edit(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<EditCommunity>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    validate_community_shape(
        form.icon.as_deref(),
        form.banner.as_deref(),
        form.discussion_languages.as_deref(),
        form.visibility.as_deref(),
    )?;
    let id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let account = plamenu_db::account::find_by_id(&state.pool, id)
        .await?
        .filter(|account| account.is_local() && account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let current_icon = crate::entities::avatar_url(&state.config.domain, &account, false);
    let current_banner = crate::entities::header_url(&state.config.domain, &account, false);
    let clear_icon = form.icon.as_deref() == Some("") && current_icon.is_some();
    let clear_banner = form.banner.as_deref() == Some("") && current_banner.is_some();
    let avatar = match form.icon.as_deref() {
        Some(url) if !url.is_empty() && Some(url) != current_icon.as_deref() => Some(
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_community_image_url"))?,
        ),
        _ => None,
    };
    let banner = match form.banner.as_deref() {
        Some(url) if !url.is_empty() && Some(url) != current_banner.as_deref() => Some(
            super::media::owned_image_bytes(&state, current.account.id, url)
                .await?
                .ok_or_else(|| LemmyError::bad_request("unsupported_community_image_url"))?,
        ),
        _ => None,
    };
    if !matches!(
        plamenu_db::group::affiliation_of(&state.pool, id, current.account.id).await?,
        Some(Affiliation::Owner | Affiliation::Moderator)
    ) {
        return Err(LemmyError::new(StatusCode::FORBIDDEN, "not_a_mod_or_admin"));
    }
    let group = plamenu_db::group::find(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let title = form.title.as_deref().unwrap_or(&account.display_name);
    let description = form.description.as_deref().unwrap_or(&account.note_source);
    let composed =
        crate::compose::compose(&state, description, crate::compose::PostFormat::Markdown)
            .await
            .map_err(LemmyError::from)?;
    crate::groups::update_settings(
        &state,
        &account,
        crate::groups::GroupSettings {
            display_name: title,
            note_html: &composed.html,
            note_source: description,
            policy: group.membership_policy(),
            sensitive: form.nsfw.unwrap_or(group.sensitive),
            posting_policy: form.posting_restricted_to_mods.map_or_else(
                || group.posting_policy(),
                |restricted| {
                    if restricted {
                        PostingPolicy::Mods
                    } else {
                        PostingPolicy::Anyone
                    }
                },
            ),
            discoverable: form
                .visibility
                .as_deref()
                .map_or(account.discoverable.unwrap_or(true), |_| true),
            profile: crate::groups::GroupProfileEdit {
                avatar,
                header: banner,
                ..crate::groups::GroupProfileEdit::default()
            },
        },
    )
    .await
    .map_err(LemmyError::from)?;
    let mut updated = plamenu_db::account::find_by_id(&state.pool, id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    if clear_icon {
        updated = crate::profile::clear_profile_image(
            &state,
            &updated,
            crate::profile::ProfileImage::Avatar,
        )
        .await
        .map_err(LemmyError::from)?;
        crate::groups::announce_group_update(&state, &updated)
            .await
            .map_err(LemmyError::from)?;
    }
    if clear_banner {
        updated = crate::profile::clear_profile_image(
            &state,
            &updated,
            crate::profile::ProfileImage::Header,
        )
        .await
        .map_err(LemmyError::from)?;
        crate::groups::announce_group_update(&state, &updated)
            .await
            .map_err(LemmyError::from)?;
    }
    Ok(Json(json!({
        "community_view": community(&state, &updated, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize, Default)]
pub struct ListCommunities {
    type_: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<ListCommunities>,
) -> Result<Json<Value>, LemmyError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let offset = (query.page.unwrap_or(1).max(1) - 1).saturating_mul(limit);
    let listing = query.type_.as_deref().unwrap_or("Local");
    let ids = match listing {
        "Subscribed" => {
            let user = current
                .as_ref()
                .ok_or_else(|| LemmyError::unauthorized("not_logged_in"))?;
            plamenu_db::group::joined_group_ids(&state.pool, user.account.id).await?
        }
        "ModeratorView" => {
            let user = current
                .as_ref()
                .ok_or_else(|| LemmyError::unauthorized("not_logged_in"))?;
            plamenu_db::group::moderated_group_ids(&state.pool, user.account.id).await?
        }
        "All" => plamenu_db::group::public_group_ids(&state.pool, limit, offset).await?,
        _ => plamenu_db::group::local_group_ids(&state.pool, false, limit, offset).await?,
    };
    let ids: Vec<i64> = if matches!(listing, "Subscribed" | "ModeratorView") {
        ids.into_iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .collect()
    } else {
        ids
    };
    let accounts = plamenu_db::account::find_by_ids(&state.pool, &ids).await?;
    let mut communities = Vec::new();
    for id in ids {
        if let Some(account) = accounts.iter().find(|account| account.id == id) {
            communities.push(
                community(
                    &state,
                    account,
                    current.as_ref().map(|user| user.account.id),
                )
                .await?,
            );
        }
    }
    Ok(Json(json!({ "communities": communities })))
}
