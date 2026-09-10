use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use plamenu_db::account::{Account, AccountSearch, ActorClass};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

use super::auth::MaybeLemmyUser;
use super::entities::{comment_view, community, person, post_view, post_view_with_read};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

async fn account_by_name(
    state: &AppState,
    name: &str,
    class: ActorClass,
) -> Result<Option<Account>, LemmyError> {
    let name = name
        .trim()
        .trim_start_matches(['@', '!'])
        .trim_end_matches('@');
    let account = match name.split_once('@') {
        None => {
            plamenu_db::account::find_public_local_account_by_username(&state.pool, name).await?
        }
        Some((username, domain)) if state.config.is_local_domain(domain) => {
            plamenu_db::account::find_public_local_account_by_username(&state.pool, username)
                .await?
        }
        Some((username, domain)) => {
            plamenu_db::account::find_remote_by_acct_class(&state.pool, username, domain, class)
                .await?
                .into_iter()
                .next()
        }
    }
    .filter(|account| match class {
        ActorClass::Any => true,
        ActorClass::Group => account.is_group(),
        ActorClass::PersonLike => !account.is_group(),
    });
    let Some(account) = account else {
        return Ok(None);
    };
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Ok(None);
    }
    Ok(Some(account))
}

async fn community_of_root(
    state: &AppState,
    status: &plamenu_db::status::Status,
) -> Result<Option<Account>, LemmyError> {
    Ok(crate::groups::communities_of_status(state, status)
        .await
        .map_err(LemmyError::from)?
        .into_iter()
        .next())
}

#[derive(Default, Deserialize)]
pub struct PersonDetails {
    person_id: Option<i32>,
    username: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
    #[serde(default)]
    saved_only: bool,
}

pub async fn person_details(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<PersonDetails>,
) -> Result<Json<Value>, LemmyError> {
    let account = match query.person_id {
        Some(id) => {
            let id = resolve_required(&state, Kind::Account, id, "person_not_found").await?;
            plamenu_db::account::find_publicly_available_by_id(&state.pool, id).await?
        }
        None => match query.username.as_deref() {
            Some(name) => account_by_name(&state, name, ActorClass::PersonLike).await?,
            None => None,
        },
    }
    .filter(|account| !account.is_group())
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    if !crate::instance_policy::account_visible(&state.pool, &state.config.domain, &account).await?
    {
        return Err(LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"));
    }
    if query.saved_only && current.is_none() {
        return Err(LemmyError::unauthorized("not_logged_in"));
    }
    let viewer = current.as_ref().map(|user| user.account.id);
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let statuses = plamenu_db::status::by_account(
        &state.pool,
        account.id,
        viewer,
        &plamenu_db::status::AccountStatusesFilter {
            exclude_reblogs: true,
            ..Default::default()
        },
        plamenu_db::user::TimelineOrder::Published,
        limit.saturating_mul(page),
    )
    .await?;
    let offset = usize::try_from((page - 1).saturating_mul(limit)).unwrap_or(usize::MAX);
    let statuses = statuses.into_iter().skip(offset).collect::<Vec<_>>();
    let read_ids = match viewer {
        Some(account_id) => {
            let ids = statuses
                .iter()
                .filter(|status| status.in_reply_to_id.is_none())
                .map(|status| status.id)
                .collect::<Vec<_>>();
            plamenu_db::post_read::read_ids(&state.pool, account_id, &ids).await?
        }
        None => std::collections::HashSet::new(),
    };
    let mut posts = Vec::new();
    let mut comments = Vec::new();
    for status in statuses {
        let view = if status.in_reply_to_id.is_some() {
            comment_view(&state, &status, viewer).await.ok()
        } else if let Some(group) = community_of_root(&state, &status).await? {
            Some(
                post_view_with_read(
                    &state,
                    &status,
                    &group,
                    viewer,
                    read_ids.contains(&status.id),
                )
                .await?,
            )
        } else {
            None
        };
        let Some(view) = view else { continue };
        if query.saved_only && view["saved"].as_bool() != Some(true) {
            continue;
        }
        if status.in_reply_to_id.is_some() {
            comments.push(view);
        } else {
            posts.push(view);
        }
    }
    let admin = account.is_local()
        && plamenu_db::role::for_account(&state.pool, account.id)
            .await?
            .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
    let group_ids = plamenu_db::group::moderated_group_ids(&state.pool, account.id).await?;
    let groups = plamenu_db::account::find_by_ids(&state.pool, &group_ids).await?;
    let mut moderates = Vec::new();
    let person_entity = person(&state, &account, admin).await?["person"].clone();
    for group in groups {
        moderates.push(json!({
            "community": community(&state, &group, viewer).await?["community"],
            "moderator": person_entity,
        }));
    }
    Ok(Json(json!({
        "person_view": person(&state, &account, admin).await?,
        "comments": comments,
        "posts": posts,
        "moderates": moderates,
    })))
}

#[derive(Deserialize)]
pub struct ResolveObject {
    q: String,
}

pub async fn resolve_object(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<ResolveObject>,
) -> Result<Json<Value>, LemmyError> {
    let viewer = current.as_ref().map(|user| user.account.id);
    let resource = if query.q.starts_with("http://") || query.q.starts_with("https://") {
        super::super::search::resolve_url(&state, &query.q, viewer).await?
    } else {
        let class = if query.q.trim_start().starts_with('!') {
            ActorClass::Group
        } else if query.q.trim_start().starts_with('@') {
            ActorClass::PersonLike
        } else {
            ActorClass::Any
        };
        account_by_name(&state, &query.q, class)
            .await?
            .map(Box::new)
            .map(super::super::search::UrlResource::Account)
    };
    let Some(resource) = resource else {
        return Err(LemmyError::new(
            StatusCode::NOT_FOUND,
            "couldnt_find_object",
        ));
    };
    let response = match resource {
        super::super::search::UrlResource::Account(account) if account.is_group() => {
            json!({ "community": community(&state, &account, viewer).await? })
        }
        super::super::search::UrlResource::Account(account) => {
            let admin = account.is_local()
                && plamenu_db::role::for_account(&state.pool, account.id)
                    .await?
                    .is_some_and(|role| role.can(plamenu_db::role::permission::ADMINISTRATOR));
            json!({ "person": person(&state, &account, admin).await? })
        }
        super::super::search::UrlResource::Status(status) if status.in_reply_to_id.is_some() => {
            json!({ "comment": comment_view(&state, &status, viewer).await? })
        }
        super::super::search::UrlResource::Status(status) => {
            let group = community_of_root(&state, &status)
                .await?
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
            json!({ "post": post_view(&state, &status, &group, viewer).await? })
        }
    };
    Ok(Json(response))
}

#[derive(Deserialize)]
pub struct Search {
    q: String,
    type_: Option<String>,
    page: Option<i64>,
    limit: Option<i64>,
    creator_id: Option<i32>,
    community_id: Option<i32>,
}

#[allow(clippy::too_many_lines)]
pub async fn search(
    State(state): State<AppState>,
    MaybeLemmyUser(current): MaybeLemmyUser,
    Query(query): Query<Search>,
) -> Result<Json<Value>, LemmyError> {
    let kind = query.type_.as_deref().unwrap_or("All");
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1).saturating_mul(limit);
    let viewer = current.as_ref().map(|user| user.account.id);
    let creator_id = match query.creator_id {
        Some(id) => Some(resolve_required(&state, Kind::Account, id, "person_not_found").await?),
        None => None,
    };
    let community_id = match query.community_id {
        Some(id) => {
            Some(resolve_required(&state, Kind::Account, id, "couldnt_find_community").await?)
        }
        None => None,
    };
    let mut users = Vec::new();
    let mut communities = Vec::new();
    if matches!(kind, "All" | "Users" | "Communities") {
        for account in plamenu_db::account::search(
            &state.pool,
            &AccountSearch {
                terms: &query.q,
                viewer,
                following: false,
                limit,
                offset,
            },
        )
        .await?
        {
            if !crate::instance_policy::public_account_visible(
                &state.pool,
                &state.config.domain,
                &account,
            )
            .await?
            {
                continue;
            }
            if account.is_group() && kind != "Users" {
                communities.push(community(&state, &account, viewer).await?);
            } else if !account.is_group() && kind != "Communities" {
                users.push(person(&state, &account, false).await?);
            }
        }
    }
    let mut posts = Vec::new();
    let mut comments = Vec::new();
    if matches!(kind, "All" | "Posts" | "Comments")
        && let Some(user) = current.as_ref()
    {
        let statuses = plamenu_db::status::search(
            &state.pool,
            &query.q,
            &plamenu_db::status::StatusSearch {
                viewer: user.account.id,
                account_id: creator_id,
                max_id: None,
                min_id: None,
                limit,
                offset,
            },
        )
        .await?;
        let read_ids = plamenu_db::post_read::read_ids(
            &state.pool,
            user.account.id,
            &statuses
                .iter()
                .filter(|status| status.in_reply_to_id.is_none())
                .map(|status| status.id)
                .collect::<Vec<_>>(),
        )
        .await?;
        for status in statuses {
            if status.in_reply_to_id.is_some() && kind != "Posts" {
                if let Ok(view) = comment_view(&state, &status, viewer).await
                    && community_id.is_none_or(|id| view["community"]["id"] == id)
                {
                    comments.push(view);
                }
            } else if status.in_reply_to_id.is_none()
                && kind != "Comments"
                && let Some(group) = community_of_root(&state, &status).await?
                && community_id.is_none_or(|id| id == group.id)
            {
                posts.push(
                    post_view_with_read(
                        &state,
                        &status,
                        &group,
                        viewer,
                        read_ids.contains(&status.id),
                    )
                    .await?,
                );
            }
        }
    }
    Ok(Json(json!({
        "type_": kind,
        "comments": comments,
        "posts": posts,
        "communities": communities,
        "users": users,
    })))
}
