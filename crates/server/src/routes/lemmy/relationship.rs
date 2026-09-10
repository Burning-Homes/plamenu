use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::LemmyUser;
use super::entities::{community, instance_id_for_domain, person};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

#[derive(Deserialize)]
pub struct FollowCommunity {
    community_id: i32,
    follow: bool,
}

pub async fn follow_community(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<FollowCommunity>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("follow").map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let target = community_account(&state, community_id).await?;
    if form.follow {
        crate::actions::follow_account(&state, &current.account, &target).await?;
    } else {
        crate::actions::unfollow_account(&state, &current.account, &target).await?;
    }
    Ok(Json(json!({
        "community_view": community(&state, &target, Some(current.account.id)).await?,
    })))
}

async fn community_account(
    state: &AppState,
    id: i64,
) -> Result<plamenu_db::account::Account, LemmyError> {
    plamenu_db::account::find_by_id(&state.pool, id)
        .await?
        .filter(plamenu_db::account::Account::is_group)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))
}

#[derive(Deserialize)]
pub struct BlockCommunity {
    community_id: i32,
    block: bool,
}

pub async fn block_community(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<BlockCommunity>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("follow").map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let target = community_account(&state, community_id).await?;
    set_account_block(&state, &current.account, &target, form.block).await?;
    Ok(Json(json!({
        "community_view": community(&state, &target, Some(current.account.id)).await?,
        "blocked": form.block,
    })))
}

async fn set_account_block(
    state: &AppState,
    actor: &plamenu_db::account::Account,
    target: &plamenu_db::account::Account,
    block: bool,
) -> Result<(), LemmyError> {
    if block {
        crate::actions::block_account(state, actor, target).await?;
    } else {
        crate::actions::unblock_account(state, actor, target).await?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct BlockPerson {
    person_id: i32,
    block: bool,
}

pub async fn block_person(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<BlockPerson>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("follow").map_err(LemmyError::from)?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(|account| !account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    set_account_block(&state, &current.account, &target, form.block).await?;
    let is_admin = target.is_local()
        && plamenu_db::role::for_account(&state.pool, target.id)
            .await?
            .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
    Ok(Json(json!({
        "person_view": person(&state, &target, is_admin).await?,
        "blocked": form.block,
    })))
}

#[derive(Deserialize)]
pub struct BlockInstance {
    instance_id: i64,
    block: bool,
}

pub async fn block_instance(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<BlockInstance>,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("follow").map_err(LemmyError::from)?;
    let instances = plamenu_db::instance_policy::known_instances(
        &state.pool,
        &plamenu_db::instance_policy::KnownInstanceFilter {
            limit: 10_000,
            ..Default::default()
        },
    )
    .await?;
    let domain = instances
        .into_iter()
        .find(|instance| instance_id_for_domain(Some(&instance.domain)) == form.instance_id)
        .map(|instance| instance.domain)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_instance"))?;
    if form.block {
        crate::actions::block_domain(&state, &current.account, &domain).await?;
    } else {
        crate::actions::unblock_domain(&state, &current.account, &domain).await?;
    }
    Ok(Json(json!({ "blocked": form.block })))
}
