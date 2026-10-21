//! A resource discussion uses ordinary status interactions inside Music.
use super::{
    i18n::Locale,
    layout, pages,
    session::{MaybeWebUser, WebUser},
    view,
};
use crate::{AppState, error::ApiError, music_activity};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
pub(super) fn link(l: Locale, kind: &str, id: i64) -> Markup {
    html! {p{a href=(format!("/music/{kind}/{id}/activities")){(l.text("music-activities-title"))}}}
}
pub async fn show(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path((kind, id)): Path<(String, i64)>,
    Query(q): Query<music_activity::Query>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(response) = super::session::preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_local,
    ) {
        return Ok(response);
    }
    if let Some(user) = &user {
        user.current.require_scope("read:statuses")?;
    }
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |u| u.locale);
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let target = music_activity::resolve(&state, &kind, id, viewer).await?;
    let page = music_activity::page(&state, &target, viewer, &q).await?;
    let back = format!("{}/activities", target.page());
    let viewer = viewer.map(|id| id.to_string());
    let ctx = view::Ctx {
        csrf: user.as_ref().map(|u| u.csrf.as_str()),
        viewer_id: viewer.as_deref(),
        return_to: &back,
        filter_context: None,
        prefs: pages::view_prefs(&state, user.as_ref()).await?,
        locale: l,
        clock: pages::clock_for(user.as_ref(), l),
        admin: user.as_ref().map_or_default(WebUser::admin_capabilities),
    };
    let body = html! {section.column.music-library{
        h1{(target.title)" · "(l.text("music-activities-title"))}
        (super::music::workspace_navigation(l,user.is_some(),target.path))
        p{a href=(target.page()){(l.text("music-activities-resource"))}}
        form.settings-form method="get" action=(back){
            label for="activity-source"{(l.text("mentions-source"))}
            select id="activity-source" name="source"{@for (value,key) in [("","notification-inbox-all"),("local","mentions-local"),("remote","mentions-federated"),("webmention","webmentions-title")]{option value=(value) selected[q.source.as_deref().unwrap_or_default()==value]{(l.text(key))}}}
            label for="activity-kind"{(l.text("music-activities-kind"))}
            select id="activity-kind" name="kind"{@for (value,key) in [("","notification-inbox-all"),("create","music-activities-posts"),("reply","webmentions-reply"),("announce","webmentions-repost"),("like","webmentions-like"),("quote","webmentions-quote"),("webmention","webmentions-title")]{option value=(value) selected[q.kind.as_deref().unwrap_or_default()==value]{(l.text(key))}}}
            button type="submit"{(l.text("music-search-submit"))}
        }
        p{a href=(format!("{back}?{}",serde_urlencoded::to_string([("source",q.source.as_deref().unwrap_or_default()),("kind",q.kind.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-activities-refresh"))}}
        @if page.items.is_empty(){p.empty{(l.text("music-activities-empty"))}}
        @for entry in &page.items{
            @if entry["kind"]=="like"{@if let Some(actor)=entry.get("account").filter(|a|!a.is_null()){p.music-activity-label{a href=(actor["url"].as_str().unwrap_or("/music")){(actor["display_name"].as_str().filter(|s|!s.is_empty()).map_or_else(||plamenu_ap::acct::display_acct(actor["acct"].as_str().unwrap_or_default()),std::borrow::Cow::Borrowed))}" · "(l.text("webmentions-like"))}}}
            @if let Some(item)=entry.get("status"){(view::status_card(&view::Status(item),&ctx))}
            @if let Some(row)=entry["id"].as_str().and_then(|s|s.parse::<i64>().ok()).and_then(|id|page.websites.get(&id)){(super::webmentions::cards(std::slice::from_ref(row),l,user.as_ref().filter(|u|u.current.has_scope("write:statuses"))))}
        }
        @if let Some(before)=page.before{a href=(format!("{back}?before={before}&{}",serde_urlencoded::to_string([("source",q.source.as_deref().unwrap_or_default()),("kind",q.kind.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("pager-older"))}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-activities-title"), user.as_ref(), &body),
    )
        .into_response())
}
