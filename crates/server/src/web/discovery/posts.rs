use super::{
    ApiError, AppState, IntoResponse, Markup, PageQuery, Query, Response, State, WebUser, browse,
    db, html, layout, pager, tabs, view,
};
use crate::discovery::{policy::Use, posts};

pub(super) async fn card(
    state: &AppState,
    entry: &db::Entry,
    user: &WebUser,
    term: &str,
    back: &str,
    images: &super::images::Images,
) -> Result<Markup, ApiError> {
    let l = user.locale;
    let actor = entry.actor_uri.as_deref().unwrap_or_default();
    let profile = crate::discovery::profiles::eligible(state, actor).await?;
    let name = profile
        .as_ref()
        .and_then(|p| p.title.as_deref())
        .unwrap_or(actor);
    let initials: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    let title = entry.title.as_deref().unwrap_or_default();
    Ok(html! {
        article.discovery-card.discovery-post id=(format!("discovery-post-{}",entry.id)) data-discovery-post=(entry.id) {
            header.discovery-card__identity {
                (super::images::avatar(images.get(actor), &html! {(initials)}))
                div {
                    h2 {a href=(actor) rel="noreferrer noopener" {(name)}}
                }
                a.discovery-original href=(&entry.canonical_uri) rel="noreferrer noopener" aria-label=(l.text("discovery-original-post")) title=(l.text("discovery-original-post")) {(view::icon("link"))}
            }
            (images.handle(actor, false))
            @if let Some(published)=entry.published_at {p.discovery-post__meta {(user.clock.element_absolute(published))}}
            @if entry.sensitive {
                details.discovery-post__warning {
                    summary {@if title.is_empty() {(l.text("discovery-sensitive"))} @else {(title)}}
                    p.discovery-post__content {(entry.summary.as_deref().unwrap_or_default())}
                }
            } @else {
                @if !title.is_empty() {p.discovery-post__title {(title)}}
                p.discovery-post__content {(entry.summary.as_deref().unwrap_or_default())}
            }
            a.discovery-connections href=(format!("/discover/trail/{}",entry.id)) {(l.text("discovery-connections"))}
            div.discovery-card__actions {
                form method="post" action=(format!("/web/discover/{}/open",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value="posts";
                    input type="hidden" name="q" value=(term);
                    input type="hidden" name="back" value=(back);
                    button type="submit" {(l.text("discovery-open-post"))}
                }
                form method="post" action=(format!("/web/discover/{}/dismiss",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value="posts";
                    input type="hidden" name="q" value=(term);
                    input type="hidden" name="back" value=(back);
                    button.secondary type="submit" {(l.text("discovery-dismiss"))}
                }
            }
        }
    })
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(mut q): Query<PageQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let l = user.locale;
    let term_owned =
        q.q.as_deref()
            .or(q.topic.as_deref())
            .unwrap_or_default()
            .trim()
            .to_owned();
    let term = term_owned.as_str();
    q.q = Some(term_owned.clone());
    q.topic = None;
    if term.len() > 512 || term.contains('\0') {
        return Err(ApiError::BadRequest("Invalid discovery query".into()));
    }
    let settings = db::settings(&state.pool).await?;
    let enabled = settings.enabled
        && !settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "public_post_indexing");
    let browsing = term.is_empty()
        && settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "recommendations");
    let page = if enabled && !browsing {
        super::discovery_page(&state, &user, &mut q, "post", term, "").await?
    } else {
        browse::Page {
            entries: Vec::new(),
            next: None,
        }
    };
    let mut cards = Vec::new();
    let images = super::images::Images::load(&state, &user, &page.entries, &[]).await?;
    for entry in &page.entries {
        cards.push(card(&state, entry, &user, term, "", &images).await?);
    }
    let next = page.next.map(|cursor| {
        format!(
            "/discover/posts?{}&{}",
            serde_urlencoded::to_string(cursor).unwrap_or_default(),
            format!(
                "{}&{}",
                serde_urlencoded::to_string([("q", term)]).unwrap_or_default(),
                q.options_query()
            )
        )
    });
    let receipt =
        super::page_receipt(&state, &user, &page.entries, !super::is_append(&headers)).await?;
    let content = html! {
        section.column.discovery data-discovery-seen-csrf=(user.csrf) data-discovery-view="posts" data-discovery-source="0" data-discovery-query=(term) {
            header.column-heading {h1 {(l.text("discovery-title"))}}
            (tabs(&user,"posts"))
            (super::discovery_controls(&user,&q,"/discover/posts","post"))
            @if !enabled {p.discovery-state role="status" {(l.text("discovery-posts-disabled"))}}
            @else if browsing {p.discovery-state role="status" {(l.text("discovery-post-browse-disabled"))}}
            @else if settings.paused {p.discovery-state role="status" {(l.text("discovery-paused"))}}
            @if q.notice.as_deref()==Some("unavailable") {p role="status" {(l.text("discovery-unavailable"))}}
            @if q.notice.as_deref()==Some("hidden") {p role="status" {(l.text("discovery-hidden"))}}
            @if page.entries.is_empty() && enabled && !browsing {p.empty {(l.text(if q.unseen_only(){"discovery-empty-unseen"}else if term.is_empty(){"discovery-posts-empty"}else{"discovery-no-matches"}))}}
            div.discovery-grid.discovery-grid--posts data-discovery-results="posts" {@for card in cards {(card)}}
            (pager(&user,next.as_deref()))
            (receipt)
        }
    };
    Ok((
        [("Cache-Control", "private, no-store"), ("Vary", "Cookie")],
        layout::shell(&l.text("discovery-title"), Some(&user), &content),
    )
        .into_response())
}

pub(super) async fn action_entry(
    state: &AppState,
    user: &WebUser,
    id: i64,
) -> Result<Option<db::Entry>, ApiError> {
    if db::entry_by_id(&state.pool, id)
        .await?
        .is_some_and(|e| e.kind == "video")
    {
        return crate::discovery::videos::eligible(
            state,
            user.current.account.id,
            id,
            Use::IndexVideo,
        )
        .await;
    }
    if db::entry_by_id(&state.pool, id)
        .await?
        .is_some_and(|e| e.kind == "post")
    {
        posts::eligible(state, user.current.account.id, id, Use::IndexPost).await
    } else {
        Ok(browse::eligible(state, user.current.account.id, id).await?)
    }
}

pub(super) async fn destination(
    state: &AppState,
    user: &WebUser,
    entry: &db::Entry,
) -> Result<Option<String>, ApiError> {
    let resolved = crate::routes::search::resolve_url(
        state,
        &entry.canonical_uri,
        Some(user.current.account.id),
    )
    .await?;
    if action_entry(state, user, entry.id).await?.is_none() {
        return Ok(None);
    }
    match resolved {
        Some(crate::routes::search::UrlResource::Account(account))
            if ["profile", "community"].contains(&entry.kind.as_str())
                && (entry.kind != "community"
                    || account.actor_type.as_deref() == Some("Group"))
                && account.uri.as_deref() == Some(entry.canonical_uri.as_str()) =>
        {
            Ok(Some(super::super::pages::account_local_path(
                &account,
                &state.config.domain,
            )))
        }
        Some(crate::routes::search::UrlResource::Status(status))
            if ["post", "video"].contains(&entry.kind.as_str())
                && status.uri.as_deref() == Some(entry.canonical_uri.as_str()) =>
        {
            let author = plamenu_db::account::find_by_id(&state.pool, status.account_id).await?;
            if author.as_ref().and_then(|a| a.uri.as_deref()) != entry.actor_uri.as_deref() {
                return Ok(None);
            }
            Ok(Some(
                super::super::pages::status_local_path(state, &status).await?,
            ))
        }
        _ => Ok(None),
    }
}
