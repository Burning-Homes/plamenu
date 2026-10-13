//! Website cards are escaped text, with explicit source links and no fake actor.
use super::{i18n::Locale, layout, session::WebUser};
use crate::{AppState, error::ApiError, webmentions};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
#[derive(Default, serde::Deserialize)]
pub struct Page {
    target: Option<String>,
    before: Option<i64>,
}
pub(super) fn cards(rows: &[webmentions::Mention], l: Locale, user: Option<&WebUser>) -> Markup {
    html! {section.webmentions aria-label=(l.text("webmentions-title")) {
        @for row in rows {@if let Some(entry)=&row.entry {
            article.webmention-card {
                @if row.photo_bytes.is_some(){img.webmention-photo src=(format!("/webmentions/{}/photo",row.id)) width="64" height="64" loading="lazy" alt="";}
                p {strong{(if entry.author.is_empty(){l.text("webmentions-website")}else{entry.author.clone()})}" · "(l.text(match entry.kind.as_str(){"reply"=>"webmentions-reply","like"=>"webmentions-like","repost"=>"webmentions-repost","quote"=>"webmentions-quote",_=>"webmentions-mention"}))}
                @if !entry.name.is_empty(){h2{(entry.name)}}
                @if !entry.summary.is_empty(){p{(entry.summary)}}
                @if let Some(date)=&entry.published {p{time datetime=(date){(date)}}}
                p{a href=(row.source) rel="nofollow noopener noreferrer" {(l.text("webmentions-open-source"))} " · " a href=(row.target){(l.text("webmentions-open-target"))}}
                @if let Some(user)=user {form.settings-form method="post" action=(format!("/web/webmentions/{}/interact",row.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    label for=(format!("website-reply-{}",row.id)){(l.text("webmentions-response"))}
                    textarea id=(format!("website-reply-{}",row.id)) name="text" maxlength="4000" {}
                    @for (kind,key) in [("reply","webmentions-publish-reply"),("like","webmentions-publish-like"),("repost","webmentions-publish-repost"),("quote","webmentions-publish-quote")] {button type="submit" name="kind" value=(kind){(l.text(key))}}
                }}
                @if !entry.categories.is_empty(){p{@for category in &entry.categories{span.music-term-links{(category)" "}}}}
            }
        }}
        @if rows.is_empty(){p.empty{(l.text("webmentions-empty"))}}
    }}
}
pub async fn archive(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:notifications")?;
    let page_result = webmentions::page(
        &state,
        Some(user.current.account.id),
        page.target.as_deref(),
        page.before,
    )
    .await?;
    let body = html! {section.column.music-library {h1{(user.locale.text("webmentions-title"))}(super::music::workspace_navigation(user.locale,true,"webmentions"))p{(user.locale.text("webmentions-hint"))}(cards(&page_result.items,user.locale,Some(&user)))@if let Some(before)=page_result.before {a href=(format!("/webmentions/archive?before={before}")){(user.locale.text("music-older"))}}}};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&user.locale.text("webmentions-title"), Some(&user), &body),
    )
        .into_response())
}
pub async fn source(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (item, author) = webmentions::outgoing::public_status(&state, id).await?;
    let l = Locale::from_headers(&headers);
    let source = format!("https://{}/webmentions/posts/{id}", state.config.domain);
    let published = crate::entities::rfc3339(item.created_at)?;
    let summary = crate::filters::plain_text(&item.content)
        .chars()
        .take(240)
        .collect::<String>();
    let categories = plamenu_db::tag::names_for_statuses(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let interaction = webmentions::interactions::get(&state, id).await?;
    let mentions = for_target(&state, &source, l).await?;
    let body = html! {article.column.h-entry {
        h1.p-name{(item.title.as_deref().unwrap_or(&summary))}
        p.p-author.h-card{a.u-url.p-name href=(crate::entities::account_web_url(&state.config.domain,&author)){(if author.display_name.is_empty(){&author.username}else{&author.display_name})}}
        a.u-url href=(source){time.dt-published datetime=(published){(published)}}
        @if let Some((target,kind))=&interaction {a class=(webmentions::interactions::marker(kind)) href=(target){(l.text("webmentions-open-target"))}}
        p.p-summary.visually-hidden{(summary)}
        @for category in &categories {span.p-category{(category)}}
        div.e-content{(maud::PreEscaped(plamenu_ap::text::sanitize_remote_html(&item.content)))}
        a href=(crate::entities::status_web_url(&state.config.domain,&item,&author.username)){(l.text("webmentions-open-source"))}
        link rel="webmention" href=(format!("https://{}/webmentions",state.config.domain));
    } (mentions)};
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::LINK,
                &format!(
                    "<https://{}/webmentions>; rel=\"webmention\"",
                    state.config.domain
                ),
            ),
        ],
        layout::shell(&l.text("webmentions-source-title"), None, &body),
    )
        .into_response())
}

/// Public cards can be included on native pages even for an authenticated owner.
/// If a resource has become private its former public mentions disappear here.
pub(super) async fn for_target(
    state: &AppState,
    target: &str,
    l: Locale,
) -> Result<Markup, ApiError> {
    match webmentions::list(state, None, Some(target), None).await {
        Ok(rows) => Ok(if rows.is_empty() {
            html! {}
        } else {
            cards(&rows, l, None)
        }),
        Err(ApiError::NotFound | ApiError::Gone) => Ok(html! {}),
        Err(e) => Err(e),
    }
}

pub async fn interact(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let fields = super::settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if !user.csrf_ok(super::settings::field(&fields, "csrf").unwrap_or_default()) {
        return Ok(super::session::csrf_rejection());
    }
    let item = webmentions::interactions::publish(
        &state,
        &user.current.account,
        id,
        super::settings::field(&fields, "kind").unwrap_or_default(),
        super::settings::field(&fields, "text").unwrap_or_default(),
    )
    .await?;
    Ok(super::settings::redirect_to(
        &crate::entities::status_web_url(
            &state.config.domain,
            &item,
            &user.current.account.username,
        ),
    ))
}
