use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use plamenu_db::account::Account;
use plamenu_db::group::Affiliation;
use plamenu_db::report::{AdminReportFilter, Report};
use plamenu_db::role::permission;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::actions::ReportParams;

use super::auth::LemmyUser;
use super::entities::{comment_view, person, post_view};
use super::error::LemmyError;
use super::ids::resolve_required;
use plamenu_db::lemmy_id::Kind;

async fn root_and_group(
    state: &AppState,
    status: &plamenu_db::status::Status,
) -> Result<(plamenu_db::status::Status, Account), LemmyError> {
    let mut root = status.clone();
    for _ in 0..40 {
        let Some(parent_id) = root.in_reply_to_id else {
            let group = crate::groups::communities_of_status(state, &root)
                .await
                .map_err(LemmyError::from)?
                .into_iter()
                .next()
                .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_community"))?;
            return Ok((root, group));
        };
        root = plamenu_db::status::find_by_id(&state.pool, parent_id)
            .await?
            .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_post"))?;
    }
    Err(LemmyError::bad_request("max_comment_depth_reached"))
}

async fn can_moderate(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    group_id: Option<i64>,
) -> Result<bool, LemmyError> {
    let staff = plamenu_db::role::for_user(&state.pool, current.user.id)
        .await?
        .is_some_and(|role| role.can(permission::MANAGE_REPORTS));
    if staff {
        return Ok(true);
    }
    let Some(group_id) = group_id else {
        return Ok(false);
    };
    Ok(matches!(
        plamenu_db::group::affiliation_of(&state.pool, group_id, current.account.id).await?,
        Some(Affiliation::Owner | Affiliation::Moderator)
    ))
}

async fn authorize(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    group_id: Option<i64>,
) -> Result<(), LemmyError> {
    if can_moderate(state, current, group_id).await? {
        Ok(())
    } else {
        Err(LemmyError::forbidden("not_a_mod_or_admin"))
    }
}

#[derive(Deserialize)]
pub struct CreateReport {
    #[serde(alias = "post_id", alias = "comment_id")]
    id: i32,
    reason: String,
}

async fn create(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    form: CreateReport,
    comment: bool,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let id = resolve_required(state, Kind::Status, form.id, "couldnt_find_object").await?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .filter(|status| status.in_reply_to_id.is_some() == comment)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_object"))?;
    let (_, group) = root_and_group(state, &status).await?;
    let target = plamenu_db::account::find_by_id(&state.pool, status.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let report = crate::actions::create_report_scoped(
        state,
        &current.account,
        &target,
        ReportParams {
            comment: &form.reason,
            category: Some("other"),
            forward: true,
            status_ids: &[status.id],
            rule_ids: None,
        },
        group.is_local().then_some(group.id),
    )
    .await
    .map_err(LemmyError::from)?;
    let view = report_view(state, &report, &status, Some(current.account.id)).await?;
    Ok(Json(if comment {
        json!({ "comment_report_view": view })
    } else {
        json!({ "post_report_view": view })
    }))
}

pub async fn create_post(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreateReport>,
) -> Result<Json<Value>, LemmyError> {
    create(&state, &current, form, false).await
}

pub async fn create_comment(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<CreateReport>,
) -> Result<Json<Value>, LemmyError> {
    create(&state, &current, form, true).await
}

async fn report_view(
    state: &AppState,
    report: &Report,
    status: &plamenu_db::status::Status,
    viewer: Option<i64>,
) -> Result<Value, LemmyError> {
    let reporter = plamenu_db::account::find_by_id(&state.pool, report.account_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "person_not_found"))?;
    let resolver = match report.action_taken_by_account_id {
        Some(id) => plamenu_db::account::find_by_id(&state.pool, id).await?,
        None => None,
    };
    let published = crate::entities::rfc3339(report.created_at).map_err(LemmyError::from)?;
    let updated = crate::entities::rfc3339(report.updated_at).map_err(LemmyError::from)?;
    let (_, group) = root_and_group(state, status).await?;
    let reporter_admin = reporter.is_local()
        && plamenu_db::role::for_account(&state.pool, reporter.id)
            .await?
            .is_some_and(|role| role.can(permission::ADMINISTRATOR));
    let reporter_person = person(state, &reporter, reporter_admin).await?["person"].clone();
    if status.in_reply_to_id.is_some() {
        let view = comment_view(state, status, viewer).await?;
        let mut out = json!({
            "comment_report": {
                "id": report.id,
                "creator_id": reporter.id,
                "comment_id": status.id,
                "original_comment_text": view["comment"]["content"],
                "reason": report.comment,
                "resolved": report.action_taken_at.is_some(),
                "resolver_id": report.action_taken_by_account_id,
                "published": published,
                "updated": updated,
            },
            "comment": view["comment"],
            "post": view["post"],
            "community": view["community"],
            "creator": reporter_person,
            "comment_creator": view["creator"],
            "counts": view["counts"],
            "creator_banned_from_community": view["creator_banned_from_community"],
            "creator_is_moderator": view["creator_is_moderator"],
            "creator_is_admin": view["creator_is_admin"],
            "creator_blocked": view["creator_blocked"],
            "subscribed": view["subscribed"],
            "saved": view["saved"],
            "my_vote": view["my_vote"],
        });
        if let Some(resolver) = resolver {
            out["resolver"] = person(state, &resolver, true).await?["person"].clone();
        }
        return Ok(out);
    }
    let view = post_view(state, status, &group, viewer).await?;
    let mut out = json!({
        "post_report": {
            "id": report.id,
            "creator_id": reporter.id,
            "post_id": status.id,
            "original_post_name": view["post"]["name"],
            "original_post_url": view["post"]["url"],
            "original_post_body": view["post"]["body"],
            "reason": report.comment,
            "resolved": report.action_taken_at.is_some(),
            "resolver_id": report.action_taken_by_account_id,
            "published": published,
            "updated": updated,
        },
        "post": view["post"],
        "community": view["community"],
        "creator": reporter_person,
        "post_creator": view["creator"],
        "creator_banned_from_community": view["creator_banned_from_community"],
        "creator_is_moderator": view["creator_is_moderator"],
        "creator_is_admin": view["creator_is_admin"],
        "subscribed": view["subscribed"],
        "saved": view["saved"],
        "read": view["read"],
        "hidden": view["hidden"],
        "creator_blocked": view["creator_blocked"],
        "my_vote": view["my_vote"],
        "unread_comments": view["unread_comments"],
        "counts": view["counts"],
    });
    if let Some(resolver) = resolver {
        out["resolver"] = person(state, &resolver, true).await?["person"].clone();
    }
    Ok(out)
}

#[derive(Default, Deserialize)]
pub struct ListReports {
    page: Option<i64>,
    limit: Option<i64>,
    unresolved_only: Option<bool>,
    community_id: Option<i32>,
    #[serde(alias = "post_id", alias = "comment_id")]
    id: Option<i32>,
}

async fn list(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    query: ListReports,
    comments: bool,
) -> Result<Json<Value>, LemmyError> {
    let community_id = match query.community_id {
        Some(id) => {
            Some(resolve_required(state, Kind::Account, id, "couldnt_find_community").await?)
        }
        None => None,
    };
    let filtered_status_id = match query.id {
        Some(id) => Some(resolve_required(state, Kind::Status, id, "couldnt_find_object").await?),
        None => None,
    };
    authorize(state, current, community_id).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let all = query.unresolved_only == Some(false);
    let reports = plamenu_db::report::list_for_admin(
        &state.pool,
        &AdminReportFilter {
            resolved: all,
            unresolved: all,
            group_account_id: community_id,
            limit: limit.saturating_mul(page),
            ..Default::default()
        },
    )
    .await?;
    let offset = usize::try_from((page - 1).saturating_mul(limit)).unwrap_or(usize::MAX);
    let mut views = Vec::new();
    for report in reports {
        let Some(status_id) = report.status_ids.first().copied() else {
            continue;
        };
        if filtered_status_id.is_some_and(|id| id != status_id) {
            continue;
        }
        let Some(status) = plamenu_db::status::find_by_id(&state.pool, status_id).await? else {
            continue;
        };
        if status.in_reply_to_id.is_some() != comments {
            continue;
        }
        views.push(report_view(state, &report, &status, Some(current.account.id)).await?);
    }
    let views = views
        .into_iter()
        .skip(offset)
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    Ok(Json(if comments {
        json!({ "comment_reports": views })
    } else {
        json!({ "post_reports": views })
    }))
}

pub async fn list_posts(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<ListReports>,
) -> Result<Json<Value>, LemmyError> {
    list(&state, &current, query, false).await
}

pub async fn list_comments(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<ListReports>,
) -> Result<Json<Value>, LemmyError> {
    list(&state, &current, query, true).await
}

#[derive(Deserialize)]
pub struct ResolveReport {
    report_id: i32,
    resolved: bool,
}

async fn resolve(
    state: &AppState,
    current: &crate::auth::CurrentUser,
    form: ResolveReport,
    comments: bool,
) -> Result<Json<Value>, LemmyError> {
    current.require_scope("write").map_err(LemmyError::from)?;
    let report_id =
        resolve_required(state, Kind::Report, form.report_id, "couldnt_find_report").await?;
    let existing = plamenu_db::report::find_by_id(&state.pool, report_id)
        .await?
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_report"))?;
    let status_id = existing
        .status_ids
        .first()
        .copied()
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_object"))?;
    let status = plamenu_db::status::find_by_id(&state.pool, status_id)
        .await?
        .filter(|status| status.in_reply_to_id.is_some() == comments)
        .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_object"))?;
    let (_, group) = root_and_group(state, &status).await?;
    authorize(state, current, existing.group_account_id.or(Some(group.id))).await?;
    let report = if form.resolved {
        plamenu_db::report::resolve(&state.pool, existing.id, current.account.id).await?
    } else {
        plamenu_db::report::unresolve(&state.pool, existing.id).await?
    }
    .ok_or_else(|| LemmyError::new(StatusCode::NOT_FOUND, "couldnt_find_report"))?;
    crate::webhooks::report_event(state, plamenu_db::webhook::REPORT_UPDATED, &report).await;
    crate::admin_log::record(
        &state.pool,
        current.account.id,
        if form.resolved { "resolve" } else { "reopen" },
        &crate::admin_log::Target::report(report.id),
    )
    .await?;
    let view = report_view(state, &report, &status, Some(current.account.id)).await?;
    Ok(Json(if comments {
        json!({ "comment_report_view": view })
    } else {
        json!({ "post_report_view": view })
    }))
}

pub async fn resolve_post(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<ResolveReport>,
) -> Result<Json<Value>, LemmyError> {
    resolve(&state, &current, form, false).await
}

pub async fn resolve_comment(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Json(form): Json<ResolveReport>,
) -> Result<Json<Value>, LemmyError> {
    resolve(&state, &current, form, true).await
}

#[derive(Default, Deserialize)]
pub struct ReportCount {
    community_id: Option<i32>,
}

pub async fn count(
    State(state): State<AppState>,
    LemmyUser(current): LemmyUser,
    Query(query): Query<ReportCount>,
) -> Result<Json<Value>, LemmyError> {
    let community_id = match query.community_id {
        Some(id) => {
            Some(resolve_required(&state, Kind::Account, id, "couldnt_find_community").await?)
        }
        None => None,
    };
    authorize(&state, &current, community_id).await?;
    let reports = plamenu_db::report::list_for_admin(
        &state.pool,
        &AdminReportFilter {
            group_account_id: community_id,
            limit: 10_000,
            ..Default::default()
        },
    )
    .await?;
    let mut posts = 0_u64;
    let mut comments = 0_u64;
    for report in reports {
        if let Some(id) = report.status_ids.first()
            && let Some(status) = plamenu_db::status::find_by_id(&state.pool, *id).await?
        {
            if status.in_reply_to_id.is_some() {
                comments += 1;
            } else {
                posts += 1;
            }
        }
    }
    Ok(Json(json!({
        "community_id": community_id,
        "post_reports": posts,
        "comment_reports": comments,
        "private_message_reports": 0,
    })))
}
