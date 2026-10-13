//! One private archive combines local, federated and website mentions.
use super::{layout, pages, session::WebUser, view};
use crate::{AppState, error::ApiError, mentions};
use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<mentions::Query>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:notifications")?;
    let page = mentions::page(&state, user.current.account.id, &q).await?;
    let l = user.locale;
    let viewer = user.current.account.id.to_string();
    let ctx = view::Ctx {
        csrf: Some(&user.csrf),
        viewer_id: Some(&viewer),
        return_to: "/mentions",
        filter_context: Some(view::FilterContext::Notifications),
        prefs: pages::view_prefs(&state, Some(&user)).await?,
        locale: l,
        clock: user.clock.clone(),
        admin: user.admin_capabilities(),
    };
    let body = html! {section.column{
        h1{(l.text("mentions-title"))}
        nav.inbox-tabs {a href="/notifications"{(l.text("page-notifications"))} a href="/conversations"{(l.text("nav-private-mentions"))}}
        p{(l.text("mentions-hint"))}
        form.settings-form method="get" action="/mentions" {
            label for="mention-source"{(l.text("mentions-source"))}
            select id="mention-source" name="source" {
                @for (source,key) in [("","notification-inbox-all"),("local","mentions-local"),("activitypub","mentions-federated"),("webmention","webmentions-title")] {option value=(source) selected[q.source.as_deref().unwrap_or_default()==source]{(l.text(key))}}
            }
            label{input type="checkbox" name="private" value="true" checked[q.private.unwrap_or(false)];" "(l.text("mentions-private"))}
            button type="submit"{(l.text("music-search-submit"))}
        }
        p{a href="/notifications/inbox"{(l.text("page-notifications"))}}
        @if page.items.is_empty(){p.empty{(l.text("mentions-empty"))}}
        @for entry in &page.items {
            @if let Some(item)=entry.get("status") {(view::status_card(&view::Status(item),&ctx))}
            @if let Some(row)=entry["id"].as_str().and_then(|id|id.parse::<i64>().ok()).and_then(|id|page.websites.get(&id)) {
                (super::webmentions::cards(std::slice::from_ref(row),l,Some(&user)))
            }
        }
        @if let Some(before)=page.before {
            a href=(format!("/mentions?before={before}&private={}&{}",q.private.unwrap_or(false),serde_urlencoded::to_string([("source",q.source.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("pager-older"))}
        }
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("mentions-title"), Some(&user), &body),
    )
        .into_response())
}
