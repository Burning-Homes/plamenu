use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use plamenu_db::account::Account;
use plamenu_db::admin_account::{AdminAccountFilter, AdminAccountView};
use plamenu_db::instance_policy::{KnownInstance, KnownInstanceFilter};
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::LemmyAdmin;
use super::entities::{community, instance_id_for_domain, person};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

#[derive(Deserialize)]
pub struct AddAdmin {
    person_id: i32,
    added: bool,
}

async fn admin_views(state: &AppState) -> Result<Vec<Value>, LemmyError> {
    let ids = plamenu_db::role::account_ids_who_can(&state.pool, permission::ADMINISTRATOR).await?;
    let accounts = plamenu_db::account::find_by_ids(&state.pool, &ids).await?;
    let mut views = Vec::with_capacity(accounts.len());
    for account in &accounts {
        views.push(person(state, account, true).await?);
    }
    Ok(views)
}

pub async fn add_admin(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<AddAdmin>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::ADMINISTRATOR, true)
        .map_err(LemmyError::from)?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(Account::is_local)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let target_user = plamenu_db::user::find_by_account_id(&state.pool, target.id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    if form.added {
        let owner = plamenu_db::role::find_by_name(&state.pool, "Owner")
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::INTERNAL_SERVER_ERROR, "unknown"))?;
        plamenu_db::role::assign_to_account(&state.pool, target.id, Some(owner.id)).await?;
    } else {
        let target_role = plamenu_db::role::for_user(&state.pool, target_user.id).await?;
        if target_role.is_some_and(|role| role.can(permission::ADMINISTRATOR))
            && plamenu_db::role::active_administrator_count(&state.pool, Some(target.id)).await?
                == 0
        {
            return Err(LemmyError::forbidden("cannot_leave_admin"));
        }
        plamenu_db::role::assign_to_account(
            &state.pool,
            target.id,
            Some(plamenu_db::role::DEFAULT_ROLE_ID),
        )
        .await?;
    }
    crate::admin_log::record(
        &state.pool,
        admin.current.account.id,
        if form.added { "promote" } else { "demote" },
        &crate::admin_log::Target::user(&target),
    )
    .await?;
    Ok(Json(json!({ "admins": admin_views(&state).await? })))
}

#[derive(Deserialize)]
pub struct BanPerson {
    person_id: i32,
    ban: bool,
    #[serde(default)]
    remove_data: bool,
    reason: Option<String>,
}

pub async fn ban_person(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<BanPerson>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, true, "accounts")
        .map_err(LemmyError::from)?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(|account| !account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    if form.ban {
        crate::moderation::apply_account_action(
            &state,
            &admin.role,
            admin.current.account.id,
            &target,
            "suspend",
            form.reason.as_deref().unwrap_or_default(),
            None,
        )
        .await
        .map_err(LemmyError::from)?;
        if form.remove_data {
            plamenu_db::status::purge_authored(&state.pool, target.id).await?;
        }
    } else {
        crate::moderation::unsuspend_account(&state, &target)
            .await
            .map_err(LemmyError::from)?;
        crate::admin_log::record(
            &state.pool,
            admin.current.account.id,
            "unsuspend",
            &crate::admin_log::Target::account(&target),
        )
        .await?;
    }
    let refreshed = plamenu_db::account::find_by_id(&state.pool, target.id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    Ok(Json(json!({
        "person_view": person(&state, &refreshed, false).await?,
        "banned": form.ban,
    })))
}

pub async fn banned_persons(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, false, "accounts")
        .map_err(LemmyError::from)?;
    let rows = plamenu_db::admin_account::list(
        &state.pool,
        &AdminAccountFilter {
            status: Some("suspended".to_owned()),
            limit: 100,
            ..Default::default()
        },
    )
    .await?;
    let mut banned = Vec::new();
    for row in rows {
        if !row.account.is_group() {
            banned.push(person(&state, &row.account, false).await?);
        }
    }
    Ok(Json(json!({ "banned": banned })))
}

#[derive(Deserialize)]
pub struct PurgePerson {
    person_id: i32,
    reason: Option<String>,
}

pub async fn purge_person(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<PurgePerson>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::DELETE_USER_DATA, true)
        .map_err(LemmyError::from)?;
    let person_id =
        resolve_required(&state, Kind::Account, form.person_id, "person_not_found").await?;
    let target = plamenu_db::account::find_by_id(&state.pool, person_id)
        .await?
        .filter(|account| !account.is_group())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let target_role = crate::moderation::target_role(&state, &target)
        .await
        .map_err(LemmyError::from)?;
    crate::moderation::authorize_account_action(
        &admin.role,
        admin.current.account.id,
        target.id,
        target_role.as_ref(),
        crate::moderation::ActionKind::Destroy,
    )
    .map_err(LemmyError::from)?;
    crate::moderation::guard_last_administrator(
        &state,
        &target,
        target_role.as_ref(),
        crate::moderation::ActionKind::Destroy,
    )
    .await
    .map_err(LemmyError::from)?;
    let _ = form.reason;
    crate::admin_log::record_account_deletion(
        &state.pool,
        target.id,
        admin.current.account.id,
        "purge",
        &crate::admin_log::Target::account(&target),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct PurgeStatus {
    #[serde(alias = "post_id", alias = "comment_id")]
    id: i32,
    reason: Option<String>,
}

async fn purge_status(
    state: &AppState,
    admin: &crate::auth::AdminUser,
    form: PurgeStatus,
    comment: bool,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::DELETE_USER_DATA, true)
        .map_err(LemmyError::from)?;
    let id = resolve_required(state, Kind::Status, form.id, "couldnt_find_object").await?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .filter(|status| status.in_reply_to_id.is_some() == comment)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_object"))?;
    let target = crate::admin_log::Target::status(status.id, comment);
    let _ = form.reason;
    plamenu_db::status::purge_by_id_with_audit(
        &state.pool,
        status.id,
        crate::admin_log::new_line(admin.current.account.id, "purge", &target),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}

pub async fn purge_post(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<PurgeStatus>,
) -> Result<Json<Value>, LemmyError> {
    purge_status(&state, &admin, form, false).await
}

pub async fn purge_comment(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<PurgeStatus>,
) -> Result<Json<Value>, LemmyError> {
    purge_status(&state, &admin, form, true).await
}

#[derive(Deserialize)]
pub struct PurgeCommunity {
    community_id: i32,
    reason: Option<String>,
}

pub async fn purge_community(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<PurgeCommunity>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::DELETE_USER_DATA, true)
        .map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let target = plamenu_db::account::find_by_id(&state.pool, community_id)
        .await?
        .filter(Account::is_group)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let _ = form.reason;
    crate::admin_log::record_account_deletion(
        &state.pool,
        target.id,
        admin.current.account.id,
        "purge",
        &crate::admin_log::Target::group(&target),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct HideCommunity {
    community_id: i32,
    hidden: bool,
    reason: Option<String>,
}

/// Applies Lemmy's instance-wide community visibility switch to Plamenu's
/// `discoverable` actor flag, then federates the updated Group actor.
pub async fn hide_community(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<HideCommunity>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_GROUPS, true, "groups")
        .map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let target = plamenu_db::account::find_by_id(&state.pool, community_id)
        .await?
        .filter(|account| account.is_group() && account.is_local())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    let updated = plamenu_db::account::update_local_profile(
        &state.pool,
        target.id,
        plamenu_db::account::ProfileUpdate {
            discoverable: Some(!form.hidden),
            ..Default::default()
        },
    )
    .await?
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    crate::groups::fan_out_group_profile(&state, &updated)
        .await
        .map_err(LemmyError::from)?;
    let _ = form.reason;
    crate::admin_log::record(
        &state.pool,
        admin.current.account.id,
        if form.hidden { "hide" } else { "unhide" },
        &crate::admin_log::Target::group(&updated),
    )
    .await?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
pub struct RemoveCommunity {
    community_id: i32,
    removed: bool,
    reason: Option<String>,
}

/// Reversibly removes a community using Plamenu's actor suspension lifecycle.
/// Unlike a hard purge, an `removed: false` request restores and re-federates
/// the actor.
pub async fn remove_community(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<RemoveCommunity>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_GROUPS, true, "groups")
        .map_err(LemmyError::from)?;
    let community_id = resolve_required(
        &state,
        Kind::Account,
        form.community_id,
        "couldnt_find_community",
    )
    .await?;
    let target = plamenu_db::account::find_by_id(&state.pool, community_id)
        .await?
        .filter(|account| account.is_group() && account.is_local())
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    if form.removed {
        plamenu_db::account::suspend(&state.pool, target.id, "local").await?;
    } else {
        plamenu_db::account::unsuspend(&state.pool, target.id).await?;
    }
    let refreshed = plamenu_db::account::find_by_id(&state.pool, target.id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
    crate::groups::fan_out_group_profile(&state, &refreshed)
        .await
        .map_err(LemmyError::from)?;
    let _ = form.reason;
    crate::admin_log::record(
        &state.pool,
        admin.current.account.id,
        if form.removed { "suspend" } else { "unsuspend" },
        &crate::admin_log::Target::group(&target),
    )
    .await?;
    Ok(Json(json!({
        "community_view": community(&state, &refreshed, Some(admin.current.account.id)).await?,
    })))
}

#[derive(Deserialize, Default)]
pub struct RegistrationList {
    page: Option<i64>,
    limit: Option<i64>,
}

async fn registration_view(
    state: &AppState,
    view: &AdminAccountView,
    deny_reason: Option<&str>,
) -> Result<Value, LemmyError> {
    let user = plamenu_db::user::find_by_account_id(&state.pool, view.account.id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let creator = person(state, &view.account, false).await?;
    Ok(json!({
        "registration_application": {
            "id": user.id,
            "local_user_id": user.id,
            "answer": view.invite_request_text.clone().unwrap_or_default(),
            "deny_reason": deny_reason,
            "published": crate::entities::rfc3339(user.created_at).map_err(LemmyError::from)?,
        },
        "creator_local_user": super::entities::local_user_view(state, &crate::auth::CurrentUser {
            user: user.clone(),
            account: view.account.clone(),
            scopes: String::new(),
            app_id: 0,
            token_id: 0,
        }).await?["local_user"],
        "creator": creator["person"],
    }))
}

pub async fn registration_count(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, false, "accounts")
        .map_err(LemmyError::from)?;
    Ok(Json(json!({
        "registration_applications": plamenu_db::account::count_local_pending(&state.pool).await?,
    })))
}

pub async fn list_registrations(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Query(query): Query<RegistrationList>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, false, "accounts")
        .map_err(LemmyError::from)?;
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let rows = plamenu_db::admin_account::list(
        &state.pool,
        &AdminAccountFilter {
            status: Some("pending".to_owned()),
            limit: limit.saturating_mul(page),
            ..Default::default()
        },
    )
    .await?;
    let mut applications = Vec::new();
    let offset = usize::try_from((page - 1).saturating_mul(limit)).unwrap_or(usize::MAX);
    for row in rows.into_iter().skip(offset) {
        applications.push(registration_view(&state, &row, None).await?);
    }
    Ok(Json(json!({ "registration_applications": applications })))
}

#[derive(Deserialize)]
pub struct RegistrationByPerson {
    person_id: i32,
}

pub async fn get_registration(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Query(query): Query<RegistrationByPerson>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, false, "accounts")
        .map_err(LemmyError::from)?;
    let person_id =
        resolve_required(&state, Kind::Account, query.person_id, "person_not_found").await?;
    let view = plamenu_db::admin_account::show(&state.pool, person_id)
        .await?
        .filter(|view| view.has_user && !view.approved)
        .ok_or_else(|| {
            LemmyError::new(
                StatusCode::NOT_FOUND,
                "couldnt_find_registration_application",
            )
        })?;
    Ok(Json(json!({
        "registration_application": registration_view(&state, &view, None).await?,
    })))
}

#[derive(Deserialize)]
pub struct ApproveRegistration {
    id: i32,
    approve: bool,
    deny_reason: Option<String>,
}

pub async fn approve_registration(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<ApproveRegistration>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require_resource(permission::MANAGE_USERS, true, "accounts")
        .map_err(LemmyError::from)?;
    let registration_id = resolve_required(
        &state,
        Kind::Registration,
        form.id,
        "couldnt_find_registration_application",
    )
    .await?;
    let user = plamenu_db::user::find_by_id(&state.pool, registration_id)
        .await?
        .filter(|user| !user.approved)
        .ok_or_else(|| {
            LemmyError::new(
                StatusCode::NOT_FOUND,
                "couldnt_find_registration_application",
            )
        })?;
    let view = plamenu_db::admin_account::show(&state.pool, user.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let response_view = registration_view(&state, &view, form.deny_reason.as_deref()).await?;
    if form.approve {
        if plamenu_db::user::approve(&state.pool, user.account_id).await? && user.confirmed() {
            let refreshed = plamenu_db::user::find_by_id(&state.pool, user.id)
                .await?
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
            crate::registration::user_became_functional(&state, &refreshed)
                .await
                .map_err(LemmyError::from)?;
        }
        crate::admin_log::record(
            &state.pool,
            admin.current.account.id,
            "approve",
            &crate::admin_log::Target::user(&view.account),
        )
        .await?;
    } else {
        crate::admin_log::record_account_deletion(
            &state.pool,
            view.account.id,
            admin.current.account.id,
            "reject",
            &crate::admin_log::Target::user(&view.account),
        )
        .await?;
    }
    Ok(Json(json!({ "registration_application": response_view })))
}

fn instance_json(instance: &KnownInstance) -> Result<Value, LemmyError> {
    Ok(json!({
        "id": instance_id_for_domain(Some(&instance.domain)),
        "domain": instance.domain,
        "published": crate::entities::rfc3339(instance.published).map_err(LemmyError::from)?,
    }))
}

async fn known_instances(state: &AppState) -> Result<Vec<KnownInstance>, LemmyError> {
    Ok(plamenu_db::instance_policy::known_instances(
        &state.pool,
        &KnownInstanceFilter {
            limit: 10_000,
            ..Default::default()
        },
    )
    .await?)
}

pub async fn federated_instances(State(state): State<AppState>) -> Result<Json<Value>, LemmyError> {
    let instances = known_instances(&state).await?;
    let mut linked = Vec::new();
    let mut allowed = Vec::new();
    let mut blocked = Vec::new();
    for instance in instances {
        let value = instance_json(&instance)?;
        if instance.block_severity.as_deref() == Some("suspend") {
            blocked.push(value);
        } else if instance.allowed {
            allowed.push(value);
        } else {
            linked.push(value);
        }
    }
    Ok(Json(json!({
        "federated_instances": { "linked": linked, "allowed": allowed, "blocked": blocked },
    })))
}
