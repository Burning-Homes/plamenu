use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use plamenu_db::account::Account;
use plamenu_db::group::Affiliation;
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::AppState;

use super::auth::LemmyUser;
use super::entities::{community, person, post_view};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

async fn group_and_authorize(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    group_id: i64,
) -> Result<Account, LemmyError> {
    let group = plamenu_db::account::find_by_id(&state.pool, group_id)
        .await?
        .filter(|account| account.is_group() && account.is_local())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let affiliation =
        plamenu_db::group::affiliation_of(&state.pool, group.id, current.account.id).await?;
    let staff = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await?
        .is_some_and(|role| role.can(permission::MANAGE_USERS));
    if !matches!(
        affiliation,
        Some(Affiliation::Owner | Affiliation::Moderator)
    ) && !staff
    {
        return Err(LemmyError::forbidden("not_a_mod_or_admin"));
    }
    Ok(group)
}

#[derive(Deserialize)]
pub struct BanFromCommunity {
    community_id: i32,
    person_id: i32,
    ban: bool,
    #[serde(default)]
    remove_data: bool,
    reason: Option<String>,
    expires: Option<i64>,
}

pub async fn ban_from_community(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<BanFromCommunity>,
) -> Result<Json<Value>, LemmyError> {
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let group = group_and_authorize(&state, &current, community_id).await?;
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(|account| !account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    if target.id == current.account.id {
        return Err(LemmyError::forbidden("cannot_block_local_instance"));
    }
    if form.ban {
        let expires = form
            .expires
            .and_then(|timestamp| OffsetDateTime::from_unix_timestamp(timestamp).ok());
        crate::groups::ban_member(
            &state,
            &group,
            &current.account,
            &target,
            expires,
            form.reason.as_deref(),
        )
        .await
        .map_err(LemmyError::from)?;
        if form.remove_data {
            plamenu_db::status::remove_group_content_of(&state.pool, group.id, target.id).await?;
        }
    } else {
        crate::groups::unban_member(&state, &group, &current.account, &target)
            .await
            .map_err(LemmyError::from)?;
    }
    Ok(Json(json!({
        "person_view": person(&state, &target, false).await?,
        "banned": form.ban,
    })))
}

#[derive(Deserialize)]
pub struct AddMod {
    community_id: i32,
    person_id: i32,
    added: bool,
}

pub async fn add_mod(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<AddMod>,
) -> Result<Json<Value>, LemmyError> {
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let group = group_and_authorize(&state, &current, community_id).await?;
    if plamenu_db::group::affiliation_of(&state.pool, group.id, current.account.id).await?
        != Some(Affiliation::Owner)
    {
        return Err(LemmyError::forbidden("not_a_mod_or_admin"));
    }
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(Account::is_local)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    crate::groups::set_moderator(&state, &group, &current.account, &target, form.added)
        .await
        .map_err(LemmyError::from)?;
    let entries = plamenu_db::group::elevated(&state.pool, group.id).await?;
    let accounts = plamenu_db::account::find_by_ids(
        &state.pool,
        &entries
            .iter()
            .map(|entry| entry.account_id)
            .collect::<Vec<_>>(),
    )
    .await?;
    let community_entity =
        community(&state, &group, Some(current.account.id)).await?["community"].clone();
    let mut moderators = Vec::new();
    for account in &accounts {
        moderators.push(json!({
            "community": community_entity,
            "moderator": person(&state, account, false).await?["person"],
        }));
    }
    Ok(Json(json!({ "moderators": moderators })))
}

async fn status_and_group(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    status_id: i64,
) -> Result<(plamenu_db::status::Status, Account), LemmyError> {
    let status = plamenu_db::status::find_by_id(&state.pool, status_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_object"))?;
    let group = crate::groups::group_accounts_of_status(state, &status)
        .await
        .map_err(LemmyError::from)?
        .pop()
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let group = group_and_authorize(state, current, group.id).await?;
    Ok((status, group))
}

#[derive(Deserialize)]
pub struct RemoveStatus {
    #[serde(alias = "post_id", alias = "comment_id")]
    id: i32,
    removed: bool,
    reason: Option<String>,
}

pub async fn remove_post(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<RemoveStatus>,
) -> Result<Json<Value>, LemmyError> {
    let id = resolve_required(&state, Kind::Status, form.id, "couldnt_find_object").await?;
    let (status, group) = status_and_group(&state, &current, id).await?;
    if status.in_reply_to_id.is_some() {
        return Err(LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"));
    }
    if form.removed {
        crate::groups::remove_from_group(
            &state,
            &group,
            &current.account,
            &status,
            form.reason.as_deref().unwrap_or_default(),
        )
        .await
        .map_err(LemmyError::from)?;
    } else {
        return Err(LemmyError::bad_request("couldnt_restore_post"));
    }
    let mut view = post_view(&state, &status, &group, Some(current.account.id)).await?;
    view["post"]["removed"] = Value::Bool(true);
    Ok(Json(json!({ "post_view": view })))
}

#[derive(Deserialize)]
pub struct LockPost {
    post_id: i32,
    locked: bool,
}

pub async fn lock_post(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<LockPost>,
) -> Result<Json<Value>, LemmyError> {
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let (status, group) = status_and_group(&state, &current, post_id).await?;
    if status.in_reply_to_id.is_some() {
        return Err(LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"));
    }
    crate::groups::set_thread_lock(&state, &group, &current.account, &status, form.locked)
        .await
        .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "post_view": post_view(&state, &status, &group, Some(current.account.id)).await?,
    })))
}

#[derive(Deserialize)]
pub struct FeaturePost {
    post_id: i32,
    featured: bool,
    feature_type: Option<String>,
}

pub async fn feature_post(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<FeaturePost>,
) -> Result<Json<Value>, LemmyError> {
    let post_id = resolve_required(&state, Kind::Status, form.post_id, "couldnt_find_post").await?;
    let (status, group) = status_and_group(&state, &current, post_id).await?;
    if form
        .feature_type
        .as_deref()
        .is_some_and(|kind| kind != "Community")
    {
        return Err(LemmyError::bad_request("couldnt_feature_post"));
    }
    crate::groups::set_group_pin(&state, &group, &current.account, &status, form.featured)
        .await
        .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "post_view": post_view(&state, &status, &group, Some(current.account.id)).await?,
    })))
}
