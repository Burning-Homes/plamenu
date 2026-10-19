use super::{
    ApiError, AppState, Form, IntoResponse, Markup, PageQuery, Path, Query, Redirect, Response,
    State, WebUser, csrf_rejection, db, html, layout, pager, server_name, tabs, view,
};
use crate::discovery::{policy::Use, videos as service};

pub(super) fn location(q: &PageQuery, notice: Option<&str>) -> String {
    let mut values = vec![("q", q.q.as_deref().unwrap_or_default().to_owned())];
    if let Some(id) = q.channel {
        values.push(("channel", id.to_string()));
    }
    if let Some(notice) = notice {
        values.push(("notice", notice.into()));
    }
    format!(
        "/discover/videos?{}&{}",
        serde_urlencoded::to_string(values).unwrap_or_default(),
        q.options_query()
    )
}
pub(super) async fn card(
    state: &AppState,
    entry: &db::Entry,
    user: &WebUser,
    back: &str,
    images: &super::images::Images,
) -> Result<Markup, ApiError> {
    let l = user.locale;
    let Some(details) = db::video_details(&state.pool, entry.id).await? else {
        return Ok(html! {});
    };
    let channel = service::eligible(state, user.current.account.id, entry.id, Use::Recommend)
        .await?
        .is_some();
    let channel_name = details
        .channel_title
        .as_deref()
        .filter(|s| channel && !s.is_empty())
        .map_or_else(|| server_name(&details.channel_uri), str::to_owned);
    Ok(html! {
        article.discovery-card.discovery-video id=(format!("discovery-video-{}",entry.id)) data-discovery-video=(entry.id) {
            header.discovery-card__identity {
                (super::images::avatar(
                    if channel {images.get(&details.channel_uri)} else {None}
                        .or_else(|| images.get(&details.owner_uri)),
                    &view::icon("video")))
                div {h2 {(entry.title.as_deref().unwrap_or_default())}
                }
                a.discovery-original href=(&entry.canonical_uri) rel="noreferrer noopener" aria-label=(l.text("discovery-original-video")) {(view::icon("link"))}
            }
            (images.handle(&details.owner_uri, false))
            @if let Some(seconds)=details.duration {p.discovery-post__meta {(format!("{}:{:02}",seconds/60,seconds%60))}}
            @if entry.sensitive {details.discovery-post__warning {summary {(l.text("discovery-sensitive"))} p.discovery-card__bio {(entry.summary.as_deref().unwrap_or_default())}}}
            @else {p.discovery-card__bio {(entry.summary.as_deref().unwrap_or_default())}}
            @if channel {a.discovery-video__channel href=(format!("/discover/videos?channel={}",entry.id)) {(channel_name)}}
            @else {p.discovery-card__server {(channel_name)}}
            a.discovery-connections href=(format!("/discover/trail/{}",entry.id)) {(l.text("discovery-connections"))}
            div.discovery-card__actions {
                form method="post" action=(format!("/web/discover/{}/open",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value="videos";
                    input type="hidden" name="back" value=(back);
                    button type="submit" {(l.text("discovery-open-video"))}
                }
                form method="post" action=(format!("/web/discover/{}/dismiss",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value="videos";
                    input type="hidden" name="back" value=(back);
                    button.secondary type="submit" aria-label=(format!("{}: {}",l.text("discovery-dismiss"),entry.title.as_deref().unwrap_or_default())) {(l.text("discovery-dismiss"))}
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
        crate::discovery::wider::term(q.q.as_deref().or(q.topic.as_deref()).unwrap_or_default())?
            .to_owned();
    let term = term_owned.as_str();
    q.q = Some(term_owned.clone());
    q.topic = None;
    let settings = db::settings(&state.pool).await?;
    let enabled = settings.enabled
        && !settings
            .disabled_mechanisms
            .iter()
            .any(|m| m == "video_indexing");
    let recommendations = !settings
        .disabled_mechanisms
        .iter()
        .any(|m| m == "recommendations");
    let channel = if let Some(id) = q.channel {
        if service::eligible(&state, user.current.account.id, id, Use::Recommend)
            .await?
            .is_some()
        {
            db::video_details(&state.pool, id).await?
        } else {
            None
        }
    } else {
        None
    };
    let page = if enabled
        && (recommendations || !term.is_empty())
        && (q.channel.is_none() || channel.is_some())
    {
        super::discovery_page(
            &state,
            &user,
            &mut q,
            "video",
            term,
            channel.as_ref().map_or("", |c| c.channel_uri.as_str()),
        )
        .await?
    } else {
        crate::discovery::browse::Page {
            entries: Vec::new(),
            next: None,
        }
    };
    let back = location(&q, None);
    let mut cards = Vec::new();
    let channel_actors: Vec<_> = channel
        .iter()
        .map(|channel| channel.channel_uri.clone())
        .collect();
    let images = super::images::Images::load(&state, &user, &page.entries, &channel_actors).await?;
    for entry in &page.entries {
        cards.push(card(&state, entry, &user, &back, &images).await?);
    }
    let next = page.next.map(|c| {
        format!(
            "{back}&{}",
            serde_urlencoded::to_string(c).unwrap_or_default()
        )
    });
    let receipt =
        super::page_receipt(&state, &user, &page.entries, !super::is_append(&headers)).await?;
    let content = html! {
        section.column.discovery data-discovery-seen-csrf=(user.csrf) data-discovery-view="videos" data-discovery-source=(q.source.unwrap_or(0)) data-discovery-query=(term) data-discovery-channel=(q.channel.unwrap_or(0)) {
            header.column-heading {h1 {(l.text("discovery-title"))}}
            (tabs(&user,"videos"))
            (super::discovery_controls(&user,&q,"/discover/videos","video"))
            @if let Some(channel)=&channel {div.discovery-topic-heading {
                (super::images::avatar(images.get(&channel.channel_uri), &view::icon("video")))
                div {h2 {(channel.channel_title.as_deref().unwrap_or_default())} a href="/discover/videos" {(l.text("discovery-all-videos"))}}
                form method="post" action=(format!("/web/discover/{}/channel",channel.entry_id)) {
                    input type="hidden" name="csrf" value=(user.csrf);button.secondary type="submit" {(l.text("discovery-open-channel"))}
                }
            }}
            @if !enabled {p.discovery-state role="status" {(l.text("discovery-videos-disabled"))}}
            @else if term.is_empty() && !recommendations{p.discovery-state role="status" {(l.text("discovery-post-browse-disabled"))}}
            @else if settings.paused {p.discovery-state role="status" {(l.text("discovery-paused"))}}
            @if q.notice.as_deref()==Some("hidden"){p.discovery-state role="status" {(l.text("discovery-hidden"))}}
            @if q.notice.as_deref()==Some("unavailable"){p.discovery-state role="status" {(l.text("discovery-unavailable"))}}
            @if enabled && page.entries.is_empty(){p.empty {(l.text(if q.unseen_only(){"discovery-empty-unseen"}else if term.is_empty(){"discovery-videos-empty"}else{"discovery-no-matches"}))}}
            div.discovery-grid data-discovery-results="videos" {@for card in cards{(card)}}
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
#[derive(serde::Deserialize)]
pub struct ChannelForm {
    csrf: String,
}
pub async fn open_channel(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ChannelForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    let back = format!("/discover/videos?channel={id}&notice=unavailable");
    if service::eligible(&state, user.current.account.id, id, Use::Recommend)
        .await?
        .is_none()
    {
        return Ok(Redirect::to(&back).into_response());
    }
    let Some(details) = db::video_details(&state.pool, id).await? else {
        return Ok(Redirect::to(&back).into_response());
    };
    if let Some(crate::routes::search::UrlResource::Account(account)) =
        crate::routes::search::resolve_url(
            &state,
            &details.channel_uri,
            Some(user.current.account.id),
        )
        .await?
        && account.uri.as_deref() == Some(details.channel_uri.as_str())
        && account.is_group()
        && service::eligible(&state, user.current.account.id, id, Use::Recommend)
            .await?
            .is_some()
    {
        return Ok(
            Redirect::to(&format!("/videos/explore?channel={}", account.id)).into_response(),
        );
    }
    Ok(Redirect::to(&back).into_response())
}
