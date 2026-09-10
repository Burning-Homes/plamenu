use axum::Json;
use axum::extract::State;
use plamenu_db::instance_settings::{GroupCreationPolicy, RegistrationsMode};
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use crate::AppState;
use crate::entities::rfc3339;

use super::auth::LemmyAdmin;
use super::auth::MaybeLemmyUser;
use super::entities::{community, instance_id_for_domain, local_user_view, person};
use super::error::LemmyError;

#[allow(
    clippy::too_many_lines,
    reason = "Lemmy's relationship bootstrap entity"
)]
async fn my_user_info(
    state: &AppState,
    current: &crate::auth::CurrentUser,
) -> Result<Value, LemmyError> {
    let me = person(state, &current.account, false).await?["person"].clone();
    let followed_ids = plamenu_db::group::joined_group_ids(&state.pool, current.account.id).await?;
    let moderated_ids =
        plamenu_db::group::moderated_group_ids(&state.pool, current.account.id).await?;
    let mut all_group_ids = followed_ids.clone();
    all_group_ids.extend(moderated_ids.iter().copied());
    let block_entries =
        plamenu_db::block::list(&state.pool, current.account.id, None, None, 10_000).await?;
    let blocked_ids = block_entries
        .iter()
        .map(|entry| entry.target_account_id)
        .collect::<Vec<_>>();
    all_group_ids.extend(blocked_ids.iter().copied());
    all_group_ids.sort_unstable();
    all_group_ids.dedup();
    let group_accounts = plamenu_db::account::find_by_ids(&state.pool, &all_group_ids).await?;
    let blocked_accounts = plamenu_db::account::find_by_ids(&state.pool, &blocked_ids).await?;

    let mut follows = Vec::new();
    for id in followed_ids {
        if let Some(group) = group_accounts.iter().find(|account| account.id == id) {
            follows.push(json!({
                "community": community(state, group, Some(current.account.id)).await?["community"],
                "follower": me,
            }));
        }
    }
    let mut moderates = Vec::new();
    for id in moderated_ids {
        if let Some(group) = group_accounts.iter().find(|account| account.id == id) {
            moderates.push(json!({
                "community": community(state, group, Some(current.account.id)).await?["community"],
                "moderator": me,
            }));
        }
    }
    let mut community_blocks = Vec::new();
    let mut person_blocks = Vec::new();
    for target in blocked_accounts {
        if target.is_group() {
            community_blocks.push(json!({
                "person": me,
                "community": community(state, &target, Some(current.account.id)).await?["community"],
            }));
        } else {
            person_blocks.push(json!({
                "person": me,
                "target": person(state, &target, false).await?["person"],
            }));
        }
    }
    let domains =
        plamenu_db::account_domain_block::list(&state.pool, current.account.id, None, None, 10_000)
            .await?;
    let known = plamenu_db::instance_policy::known_instances(
        &state.pool,
        &plamenu_db::instance_policy::KnownInstanceFilter {
            limit: 10_000,
            ..Default::default()
        },
    )
    .await?;
    let mut instance_blocks = Vec::new();
    for entry in domains {
        let published = known
            .iter()
            .find(|instance| instance.domain == entry.domain)
            .map_or_else(OffsetDateTime::now_utc, |instance| instance.published);
        instance_blocks.push(json!({
            "person": me,
            "instance": {
                "id": instance_id_for_domain(Some(&entry.domain)),
                "domain": entry.domain,
                "published": rfc3339(published).map_err(LemmyError::from)?,
            },
        }));
    }
    Ok(json!({
        "local_user_view": local_user_view(state, current).await?,
        "follows": follows,
        "moderates": moderates,
        "community_blocks": community_blocks,
        "instance_blocks": instance_blocks,
        "person_blocks": person_blocks,
        "discussion_languages": [1],
    }))
}

#[allow(
    clippy::too_many_lines,
    reason = "Lemmy's site response is one nested wire entity"
)]
pub async fn get_site(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
) -> Result<Json<Value>, LemmyError> {
    let settings = plamenu_db::instance_settings::get(&state.pool).await?;
    let now = OffsetDateTime::now_utc();
    let users = plamenu_db::account::count_public_local(&state.pool)
        .await?
        .cast_signed();
    let (posts, comments) = plamenu_db::status::count_local_posts_comments(&state.pool).await?;
    let communities = plamenu_db::group::count_local(&state.pool).await?;
    let active_day =
        plamenu_db::metrics::active_users_total(&state.pool, now - Duration::days(1), now).await?;
    let active_week =
        plamenu_db::metrics::active_users_total(&state.pool, now - Duration::days(7), now).await?;
    let active_month =
        plamenu_db::metrics::active_users_total(&state.pool, now - Duration::days(30), now).await?;
    let active_half_year =
        plamenu_db::metrics::active_users_total(&state.pool, now - Duration::days(182), now)
            .await?;
    let published = rfc3339(now).map_err(LemmyError::from)?;
    let thumbnail = super::super::api::site_thumbnail(&state)
        .await
        .map_err(LemmyError::from)?;
    let registration_mode = match settings.registrations_mode() {
        RegistrationsMode::Open => "Open",
        RegistrationsMode::Approved => "RequireApplication",
        RegistrationsMode::None => "Closed",
    };
    let site_view = json!({
        "site": {
            "id": 1,
            "name": settings.site_title,
            "sidebar": settings.site_extended_description,
            "published": published,
            "icon": thumbnail.as_ref().map(|image| image.url_1x.as_str()),
            "description": settings.site_short_description,
            "actor_id": format!("https://{}/actor", state.config.domain),
            "inbox_url": format!("https://{}/inbox", state.config.domain),
            "instance_id": 1,
        },
        "local_site": {
            "id": 1,
            "site_id": 1,
            "site_setup": true,
            "enable_downvotes": true,
            "enable_nsfw": true,
            "community_creation_admin_only": false,
            "require_email_verification": true,
            "private_instance": false,
            "default_post_listing_type": "All",
            "federation_enabled": true,
            "captcha_enabled": false,
            "registration_mode": registration_mode,
            "federation_signed_fetch": state.config.authorized_fetch,
            "default_sort_type": "Active",
        },
        "local_site_rate_limit": {
            "local_site_id": 1,
            "message": settings.rate_limit_authenticated_api,
            "message_per_second": 60,
            "post": settings.rate_limit_authenticated_api,
            "post_per_second": 60,
            "register": settings.rate_limit_api_sign_up,
            "register_per_second": 60,
            "image": settings.rate_limit_api_media,
            "image_per_second": 60,
            "comment": settings.rate_limit_authenticated_api,
            "comment_per_second": 60,
            "search": settings.rate_limit_authenticated_api,
            "search_per_second": 60,
            "published": published,
        },
        "counts": {
            "site_id": 1,
            "users": users,
            "posts": posts,
            "comments": comments,
            "communities": communities,
            "users_active_day": active_day,
            "users_active_week": active_week,
            "users_active_month": active_month,
            "users_active_half_year": active_half_year,
        },
    });
    let admin_ids = plamenu_db::role::account_ids_who_can(
        &state.pool,
        plamenu_db::role::permission::ADMINISTRATOR,
    )
    .await?;
    let admin_accounts = plamenu_db::account::find_by_ids(&state.pool, &admin_ids).await?;
    let mut admins = Vec::with_capacity(admin_accounts.len());
    for account in &admin_accounts {
        admins.push(person(&state, account, true).await?);
    }
    let custom_emojis = super::custom_emoji::views(
        &state,
        &plamenu_db::custom_emoji::listed(&state.pool).await?,
    )
    .await?;
    let my_user = if let Some(current) = current.as_ref() {
        Some(my_user_info(&state, current).await?)
    } else {
        None
    };
    Ok(Json(json!({
        "site_view": site_view,
        "admins": admins,
        "version": super::LEMMY_COMPAT_VERSION,
        "my_user": my_user,
        "all_languages": [{ "id": 1, "code": "en", "name": "English" }],
        "discussion_languages": [1],
        "taglines": [],
        "custom_emojis": custom_emojis,
        "blocked_urls": [],
    })))
}

#[derive(Deserialize, Default)]
pub struct EditSite {
    name: Option<String>,
    sidebar: Option<String>,
    description: Option<String>,
    enable_downvotes: Option<bool>,
    community_creation_admin_only: Option<bool>,
    require_email_verification: Option<bool>,
    private_instance: Option<bool>,
    rate_limit_message: Option<i32>,
    rate_limit_message_per_second: Option<i32>,
    rate_limit_post: Option<i32>,
    rate_limit_post_per_second: Option<i32>,
    rate_limit_register: Option<i32>,
    rate_limit_register_per_second: Option<i32>,
    rate_limit_image: Option<i32>,
    rate_limit_image_per_second: Option<i32>,
    rate_limit_comment: Option<i32>,
    rate_limit_comment_per_second: Option<i32>,
    rate_limit_search: Option<i32>,
    rate_limit_search_per_second: Option<i32>,
    registration_mode: Option<String>,
    federation_enabled: Option<bool>,
    captcha_enabled: Option<bool>,
    #[serde(flatten)]
    unsupported: std::collections::HashMap<String, Value>,
}

fn per_minute(count: Option<i32>, period_seconds: Option<i32>, current: i32) -> i32 {
    let count = count.unwrap_or(current).max(1);
    match period_seconds {
        Some(period) if period > 0 => count.saturating_mul(60).saturating_div(period).max(1),
        _ => count,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "field-for-field adapter into the full settings update"
)]
pub async fn edit_site(
    State(state): State<AppState>,
    LemmyAdmin(admin): LemmyAdmin,
    Json(form): Json<EditSite>,
) -> Result<Json<Value>, LemmyError> {
    admin
        .require(permission::ADMINISTRATOR, true)
        .map_err(LemmyError::from)?;
    if !form.unsupported.is_empty()
        || form.enable_downvotes == Some(false)
        || form.require_email_verification == Some(false)
        || form.federation_enabled == Some(false)
        || form.captcha_enabled == Some(true)
    {
        // These settings have no honest Plamenu equivalent yet. Rejecting is
        // intentional: an admin client must never be told a mutation succeeded
        // when the instance could not enforce it.
        return Err(LemmyError::bad_request("invalid_admin_setting"));
    }
    let current = plamenu_db::instance_settings::get(&state.pool).await?;
    let title = form.name.as_deref().unwrap_or(&current.site_title);
    let sidebar = form
        .sidebar
        .as_deref()
        .unwrap_or(&current.site_extended_description);
    let description = form
        .description
        .as_deref()
        .unwrap_or(&current.site_short_description);
    let registrations_mode = match form.registration_mode.as_deref() {
        Some("Open") => RegistrationsMode::Open,
        Some("RequireApplication") => RegistrationsMode::Approved,
        Some("Closed") => RegistrationsMode::None,
        Some(_) => return Err(LemmyError::bad_request("invalid_registration_mode")),
        None => current.registrations_mode(),
    };
    let authenticated_rate = [
        per_minute(
            form.rate_limit_message,
            form.rate_limit_message_per_second,
            current.rate_limit_authenticated_api,
        ),
        per_minute(
            form.rate_limit_post,
            form.rate_limit_post_per_second,
            current.rate_limit_authenticated_api,
        ),
        per_minute(
            form.rate_limit_comment,
            form.rate_limit_comment_per_second,
            current.rate_limit_authenticated_api,
        ),
        per_minute(
            form.rate_limit_search,
            form.rate_limit_search_per_second,
            current.rate_limit_authenticated_api,
        ),
    ]
    .into_iter()
    .max()
    .unwrap_or(current.rate_limit_authenticated_api);
    let mut update = current.as_update();
    update.site_title = title;
    update.site_short_description = description;
    update.site_extended_description = sidebar;
    update.registrations_mode = registrations_mode;
    update.rate_limit_authenticated_api = authenticated_rate;
    update.rate_limit_api_media = per_minute(
        form.rate_limit_image,
        form.rate_limit_image_per_second,
        current.rate_limit_api_media,
    );
    update.rate_limit_api_sign_up = per_minute(
        form.rate_limit_register,
        form.rate_limit_register_per_second,
        current.rate_limit_api_sign_up,
    );
    if let Some(admin_only) = form.community_creation_admin_only {
        update.group_creation_policy = if admin_only {
            GroupCreationPolicy::Admins
        } else {
            GroupCreationPolicy::Everyone
        };
    }
    if let Some(private) = form.private_instance {
        update.timeline_preview_federated = !private;
        update.timeline_preview_local = !private;
        update.timeline_preview_tag = !private;
        update.public_search = !private;
        update.anon_trends = !private;
        update.anon_directory = !private;
        update.anon_directory_federated = !private;
        update.anon_groups = !private;
    }
    plamenu_db::instance_settings::save(&state.pool, update).await?;
    state.settings_cache.invalidate();
    plamenu_db::admin_action_log::record(
        &state.pool,
        plamenu_db::admin_action_log::NewActionLog {
            account_id: admin.current.account.id,
            action: "update",
            target_type: "InstanceSettings",
            target_id: 1,
            human_identifier: &state.config.domain,
            permalink: Some("/admin/settings"),
        },
    )
    .await?;
    let response = get_site(State(state), MaybeLemmyUser(None)).await?.0;
    Ok(Json(json!({
        "site_view": response["site_view"],
        "taglines": response["taglines"],
    })))
}
