use super::WebAdmin;
use crate::{AppState, music_jobs};
use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::role::permission;
pub async fn index(
    State(state): State<AppState>,
    admin: WebAdmin,
    Query(q): Query<music_jobs::Query>,
) -> Result<Response, Response> {
    admin.require(permission::VIEW_DEVOPS)?;
    let page = music_jobs::page(&state, &q)
        .await
        .map_err(IntoResponse::into_response)?;
    let l = admin.user.locale;
    let title = l.text("music-jobs-title");
    let body = html! {section.column{p{(l.text("music-jobs-hint"))}p{a href="/admin/music/jobs"{(l.text("music-activities-refresh"))}}
        table{caption{(l.text("music-jobs-counts"))}thead{tr{th scope="col"{(l.text("music-jobs-kind"))}th scope="col"{(l.text("music-jobs-state"))}th scope="col"{(l.text("music-jobs-count"))}}}tbody{@for c in &page.counts{tr{td{(l.text(&format!("music-jobs-kind-{}",c.kind)))}td{(l.text(&format!("music-jobs-state-{}",c.state)))}td{(c.count)}}}}}
        @if page.items.is_empty(){p{(l.text("music-jobs-empty"))}}
        form method="post" action="/web/admin/music/jobs/cancel"{
            input type="hidden" name="csrf" value=(admin.user.csrf);
            ul{@for item in &page.items{li{
                @let ref_id=format!("{}:{}",item.kind,item.id);
                @if item.kind!="recovery" && admin.role.can(permission::MANAGE_SETTINGS){input id=(format!("job-{}",ref_id)) type="checkbox" name="jobs" value=(ref_id);}
                @if item.kind!="recovery" && admin.role.can(permission::MANAGE_SETTINGS){label for=(format!("job-{}",ref_id)){(l.text(&format!("music-jobs-kind-{}",item.kind)))" · "(item.id)" · "(l.text("music-jobs-owner"))" "(item.owner_id)" · "(l.text(&format!("music-jobs-state-{}",item.state)))}}
                @else{span{(l.text(&format!("music-jobs-kind-{}",item.kind)))" · "(item.id)" · "(l.text("music-jobs-owner"))" "(item.owner_id)" · "(l.text(&format!("music-jobs-state-{}",item.state)))}}
                @if item.kind=="recovery"{p{(l.text("music-folders-recovery-pending"))}}
                @else if admin.role.can(permission::MANAGE_SETTINGS){button type="submit" name="task" value=(ref_id){(l.text("music-jobs-cancel"))}}
            }}}
            @if admin.role.can(permission::MANAGE_SETTINGS) && page.items.iter().any(|item|item.kind!="recovery"){button type="submit"{(l.text("music-jobs-cancel-selected"))}}
        }
        @if let Some(before)=page.before{a href=(format!("/admin/music/jobs?before={before}")){(l.text("pager-older"))}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        super::admin_shell(&admin, "/admin/music/jobs", &title, &body),
    )
        .into_response())
}
pub async fn cancel(
    State(state): State<AppState>,
    admin: WebAdmin,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    admin.require(permission::MANAGE_SETTINGS)?;
    let fields = crate::web::settings::form_pairs(&body)
        .map_err(|e| crate::error::ApiError::Unprocessable(e).into_response())?;
    if !admin
        .user
        .csrf_ok(crate::web::settings::field(&fields, "csrf").unwrap_or_default())
    {
        return Ok(crate::web::session::csrf_rejection());
    }
    let references = if let Some(task) = crate::web::settings::field(&fields, "task") {
        vec![task]
    } else {
        fields
            .iter()
            .filter(|(key, _)| key == "jobs")
            .map(|(_, value)| value.as_str())
            .collect()
    };
    let references =
        if references.is_empty() && crate::web::settings::field(&fields, "kind").is_some() {
            vec![music_jobs::Reference {
                kind: crate::web::settings::field(&fields, "kind")
                    .unwrap_or_default()
                    .into(),
                id: crate::web::settings::field(&fields, "id")
                    .unwrap_or_default()
                    .into(),
            }]
        } else {
            references
                .into_iter()
                .map(|value| {
                    let (kind, id) = value.split_once(':').unwrap_or(("", ""));
                    music_jobs::Reference {
                        kind: kind.into(),
                        id: id.into(),
                    }
                })
                .collect()
        };
    music_jobs::cancel_many(&state, admin.user.current.account.id, &references)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(crate::web::settings::redirect_to("/admin/music/jobs"))
}
