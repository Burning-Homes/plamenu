//! The signed-in discovery journey. Rendering never acquires remote resources.
use axum::{
    Form,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect, Response},
};
use maud::{Markup, html};
use plamenu_db::{account_domain_block, discovery_store as db};
use serde::Deserialize;

use super::{
    layout,
    session::{WebUser, csrf_rejection},
    view,
};
use crate::{AppState, discovery::browse, error::ApiError};

mod images;
mod posts;
mod topics;
mod videos;
mod wider;
pub use posts::index as post_index;
pub use topics::{collection as collection_trail, index as topic_index, save as save_topic, trail};
pub use videos::{index as video_index, open_channel};
pub use wider::search as wider_search;

#[derive(Default, Deserialize)]
pub struct PageQuery {
    #[serde(skip)]
    unseen_exhausted: bool,
    journey: Option<i64>,
    sort: Option<String>,
    language: Option<String>,
    seen: Option<bool>,
    rotation: Option<i64>,
    source_order: Option<i64>,
    watermark: Option<i64>,
    ordinal: Option<i64>,
    creator_ordinal: Option<i64>,
    after: Option<i64>,
    source: Option<i64>,
    sources: Option<bool>,
    notice: Option<String>,
    q: Option<String>,
    topic: Option<String>,
    search: Option<i64>,
    channel: Option<i64>,
}

impl PageQuery {
    fn cursor(&self) -> Result<Option<browse::Cursor>, ApiError> {
        match (self.watermark, self.ordinal, self.after) {
            (None, None, None) => Ok(None),
            (Some(watermark), Some(ordinal), Some(after))
                if watermark >= 0
                    && ordinal >= 0
                    && self.creator_ordinal.unwrap_or(0) >= 0
                    && after >= 0
                    && after <= watermark =>
            {
                Ok(Some(browse::Cursor {
                    journey: self.journey,
                    rotation: self.rotation.unwrap_or(0),
                    source_order: self.source_order.unwrap_or(0),
                    watermark,
                    ordinal,
                    creator_ordinal: self.creator_ordinal.unwrap_or(0),
                    after,
                }))
            }
            _ => Err(ApiError::BadRequest("Invalid discovery cursor".into())),
        }
    }
}

pub(super) fn server_name(origin: &str) -> String {
    url::Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| origin.to_owned())
}

fn card(entry: &db::Entry, user: &WebUser, back: &str, images: &images::Images) -> Markup {
    let l = user.locale;
    let community = entry.kind == "community";
    let name = entry.title.as_deref().unwrap_or_default();
    let initials: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    html! {
        article.discovery-card id=(format!("discovery-profile-{}",entry.id)) data-discovery-card=(entry.id) {
            header.discovery-card__identity {
                (images::avatar(images.get(&entry.canonical_uri), &html! {(initials)}))
                div {
                    h2 {(name)}
                }
                a.discovery-original href=(&entry.canonical_uri) rel="noreferrer noopener"
                    aria-label=(format!("{}: {name}",l.text(if community {"discovery-original-community"} else {"discovery-original"}))) title=(l.text(if community {"discovery-original-community"} else {"discovery-original"})) {(view::icon("link"))}
            }
            (images.handle(&entry.canonical_uri, community))
            @if let Some(bio)=entry.summary.as_deref().filter(|s|!s.is_empty()) {
                @if entry.sensitive {details.discovery-post__warning {summary {(l.text("discovery-sensitive"))} p.discovery-card__bio {(bio)}}} @else {p.discovery-card__bio {(bio)}}
            }
            a.discovery-connections href=(format!("/discover/trail/{}",entry.id)) {(l.text("discovery-connections"))}
            div.discovery-card__actions {
                form method="post" action=(format!("/web/discover/{}/open",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value=(if community {"communities"} else {"people"});
                    input type="hidden" name="back" value=(back);
                    button type="submit" {(l.text(if community {"discovery-open-community"} else {"discovery-open"}))}
                }
                form method="post" action=(format!("/web/discover/{}/dismiss",entry.id)) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="view" value=(if community {"communities"} else {"people"});
                    input type="hidden" name="back" value=(back);
                    button.secondary type="submit" aria-label=(format!("{}: {name}",l.text("discovery-dismiss"))) {(l.text("discovery-dismiss"))}
                }
            }
        }
    }
}

fn pager(user: &WebUser, next: Option<&str>) -> Markup {
    let l = user.locale;
    html! {
        div.discovery-pagination {
            @if let Some(href)=next {
                a.discovery-more href=(href) {(l.text("discovery-more"))}
            }
            p.discovery-page-status role="status" aria-live="polite" aria-atomic="true"
                data-loading=(l.text("discovery-page-loading")) data-error=(l.text("discovery-page-error"))
                data-loaded=(l.text("discovery-page-loaded")) data-end=(l.text("discovery-page-end")) {}
        }
    }
}

async fn sources(state: &AppState, user: &WebUser, after: i64) -> Result<Markup, ApiError> {
    let l = user.locale;
    let domains = account_domain_block::all_domains(&state.pool, user.current.account.id).await?;
    let candidates = db::browse_sources(&state.pool, after).await?;
    let full = candidates.len() == 200;
    let mut scanned = after;
    let mut visible = Vec::new();
    for source in candidates {
        scanned = source.id;
        if browse::source_visible(state, &source, &domains).await? {
            visible.push(source);
            if visible.len() == 21 {
                break;
            }
        }
    }
    let next = if visible.len() > 20 {
        visible.get(19).map(|s| s.id)
    } else {
        full.then_some(scanned)
    };
    visible.truncate(20);
    let actors: Vec<_> = visible
        .iter()
        .flat_map(|s| {
            [
                s.origin.clone(),
                format!("{}/", s.origin),
                format!("{}/actor", s.origin),
            ]
        })
        .collect();
    let known_images = images::Images::load(state, user, &[], &actors).await?;
    let mut server_images = std::collections::HashMap::new();
    for source in &visible {
        if db::server_icon(&state.pool, source.id)
            .await?
            .is_some_and(|i| i.image.is_some())
        {
            server_images.insert(
                source.id,
                format!("/web/discover/servers/{}/icon", source.id),
            );
        }
    }
    let next = next.map(|after| format!("/discover?sources=true&source={after}"));
    Ok(html! {
        @if visible.is_empty() {p.empty {(l.text("discovery-empty-sources"))}}
        div.discovery-grid data-discovery-results="servers" {
            @for source in &visible {
                article.discovery-card.discovery-server id=(format!("discovery-server-{}",source.id)) data-discovery-server=(source.id) {
                    header.discovery-card__identity {
                        (images::avatar(server_images.get(&source.id).map(String::as_str).or_else(||known_images.get(&source.origin)).or_else(||known_images.get(&format!("{}/",source.origin))).or_else(||known_images.get(&format!("{}/actor",source.origin))), &view::icon("explore")))
                        div {h2 {(server_name(&source.origin))}}
                    }
                    div.discovery-card__actions {
                        @let community=source.capabilities.iter().any(|c|c=="community_directory");
                        @let video=source.capabilities.iter().any(|c|c=="video_catalogue");
                        a.button href=(format!("{}?source={}",if video {"/discover/videos"} else if community {"/discover/communities"} else {"/discover"},source.id)) {(l.text(if video {"discovery-browse-videos"} else if community {"discovery-browse-communities"} else {"discovery-browse-source"}))}
                        a.discovery-visit href=(&source.origin) rel="noreferrer noopener" {(l.text("discovery-visit")) " " (view::icon("link"))}
                    }
                }
            }
        }
        (pager(user,next.as_deref()))
    })
}

fn tabs(user: &WebUser, selected: &str) -> Markup {
    let l = user.locale;
    html! {nav.discovery-tabs aria-label=(l.text("discovery-tabs")) {
        @for (href,label,name) in [("/discover","discovery-people","people"),("/discover/posts","discovery-posts","posts"),("/discover/communities","discovery-communities","communities"),("/discover/videos","discovery-videos","videos"),("/discover?sources=true","discovery-servers","servers"),("/discover/topics","discovery-topics","topics")] {
            a href=(href) aria-current=[(selected==name).then_some("page")] {(l.text(label))}
        }
    }}
}

pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    index_inner(state, user, q, false, !is_append(&headers)).await
}

pub async fn community_index(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<PageQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    index_inner(state, user, q, true, !is_append(&headers)).await
}

async fn index_inner(
    state: AppState,
    user: WebUser,
    mut q: PageQuery,
    community: bool,
    record_page: bool,
) -> Result<Response, ApiError> {
    let base = if community {
        "/discover/communities"
    } else {
        "/discover"
    };
    let view = if community { "communities" } else { "people" };
    let l = user.locale;
    let term_owned =
        crate::discovery::wider::term(q.q.as_deref().or(q.topic.as_deref()).unwrap_or_default())?
            .to_owned();
    let term = term_owned.as_str();
    q.q = Some(term_owned.clone());
    q.topic = None;
    let settings = db::settings(&state.pool).await?;
    let diagnostic = db::diagnostics(&state.pool).await?;
    let queue = db::queue_diagnostics(&state.pool).await?;
    let status = browse::status_for(
        &settings,
        &diagnostic,
        &queue,
        if community {
            "community_directory"
        } else {
            "directory"
        },
    );
    let cursor = q.cursor()?;
    let show_sources = !community && q.sources.unwrap_or(false);
    let report = wider::report(&state, &user, &q, community).await?;
    let search_controls = if show_sources || status == "discovery-disabled" {
        html! {}
    } else {
        wider::controls(&state, &user, &q, community).await?
    };
    let cards = if status == "discovery-disabled" {
        html! {}
    } else if show_sources {
        sources(&state, &user, q.source.unwrap_or(0)).await?
    } else {
        let mut page = discovery_page(
            &state,
            &user,
            &mut q,
            if community { "community" } else { "profile" },
            term,
            "",
        )
        .await?;
        if cursor.is_none() {
            for entry in &report.entries {
                if !page.entries.iter().any(|e| e.id == entry.id) {
                    page.entries.push(entry.clone());
                }
            }
        }
        let back = wider::location(&q, community, None);
        let images = images::Images::load(&state, &user, &page.entries, &[]).await?;
        let receipt = page_receipt(&state, &user, &page.entries, record_page).await?;
        html! {
            (receipt)
            @if q.source.is_some() {p {a href=(base) {(l.text("discovery-all"))}}}
            @if page.entries.is_empty() {p.empty {(l.text(if q.unseen_only() {"discovery-empty-unseen"} else if community {"discovery-empty-communities"} else {"discovery-empty"}))}}
            div.discovery-grid data-discovery-results=(view) {@for entry in &page.entries {(card(entry,&user,&back,&images))}}
            @if let Some(next)=page.next {
                @let href=format!("{}&{}",wider::location(&q,community,None),serde_urlencoded::to_string(next).unwrap_or_default());
                (pager(&user,Some(&href)))
            }
        }
    };
    let content = html! {
        section.column.discovery data-discovery-seen-csrf=(user.csrf) data-discovery-view=(if show_sources {"servers"}else{view}) data-discovery-source=(q.source.unwrap_or(0)) data-discovery-query=(term) {
            header.column-heading {h1 {(l.text("discovery-title"))}}
            (tabs(&user,if show_sources {"servers"}else{view}))
            @if status != "discovery-ready" {
                p.discovery-state role="status" data-discovery-state=(status) {(l.text(status))}
            }
            @if q.notice.as_deref()==Some("unavailable") {p role="status" {(l.text("discovery-unavailable"))}}
            @if q.notice.as_deref()==Some("hidden") {p role="status" {(l.text("discovery-hidden"))}}
            @if !show_sources {(discovery_controls(&user,&q,base,if community {"community"} else {"profile"}))}
            (search_controls)
            (report.markup)
            (cards)
        }
    };
    Ok((
        [("Cache-Control", "private, no-store"), ("Vary", "Cookie")],
        layout::shell(&l.text("discovery-title"), Some(&user), &content),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ActionForm {
    back: Option<String>,
    csrf: String,
    view: Option<String>,
    q: Option<String>,
}

pub async fn dismiss(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    if posts::action_entry(&state, &user, id).await?.is_some() {
        db::remember_seen(&state.pool, user.current.account.id, &[id], false).await?;
        db::dismiss(&state.pool, user.current.account.id, id).await?;
    }
    Ok(Redirect::to(&action_back(&form, "hidden")).into_response())
}

fn action_back(form: &ActionForm, notice: &str) -> String {
    if let Some(raw) = form
        .back
        .as_deref()
        .and_then(|s| s.strip_prefix("/search?"))
        && let Ok(q) = serde_urlencoded::from_str::<PageQuery>(raw)
        && let Ok(term) = crate::discovery::wider::term(q.q.as_deref().unwrap_or_default())
    {
        return format!(
            "/search?{}",
            serde_urlencoded::to_string([("q", term)]).unwrap_or_default()
        );
    }

    if form.back.as_deref() == Some("/discover/videos") {
        return format!("/discover/videos?notice={notice}");
    }
    if let Some(raw) = form
        .back
        .as_deref()
        .and_then(|b| b.strip_prefix("/discover/videos?"))
        && let Ok(q) = serde_urlencoded::from_str::<PageQuery>(raw)
        && crate::discovery::wider::term(q.q.as_deref().unwrap_or_default()).is_ok()
    {
        return videos::location(&q, Some(notice));
    }
    if form.view.as_deref() == Some("videos") && form.back.as_deref().is_none_or(str::is_empty) {
        return format!("/discover/videos?notice={notice}");
    }
    if let Some((base, query)) = form.back.as_deref().and_then(|b| b.split_once('?'))
        && ["/discover", "/discover/communities"].contains(&base)
        && let Ok(q) = serde_urlencoded::from_str::<PageQuery>(query)
        && crate::discovery::wider::term(q.q.as_deref().unwrap_or_default()).is_ok()
    {
        return wider::location(&q, base == "/discover/communities", Some(notice));
    }
    // Only our own topic/collection views, never an arbitrary return URL.
    if let Some(back) = form.back.as_deref() {
        if back == "/discover/topics" {
            return format!("{back}?notice={notice}");
        }
        if let Some(query) = back.strip_prefix("/discover/topics?")
            && let Ok(q) = serde_urlencoded::from_str::<PageQuery>(query)
            && let Some(topic) = q.topic.as_deref().and_then(db::normalize_topic)
        {
            return format!(
                "/discover/topics?{}&notice={notice}",
                serde_urlencoded::to_string([("topic", topic)]).unwrap_or_default()
            );
        }
        if let Some(id) = back
            .strip_prefix("/discover/collections/")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|id| *id > 0)
        {
            return format!("/discover/collections/{id}?notice={notice}");
        }
    }
    if form.view.as_deref() == Some("posts") {
        format!(
            "/discover/posts?{}",
            serde_urlencoded::to_string([
                ("notice", notice),
                (
                    "q",
                    form.q
                        .as_deref()
                        .filter(|q| q.len() <= 512 && !q.contains('\0'))
                        .unwrap_or_default()
                )
            ])
            .unwrap_or_default()
        )
    } else if form.view.as_deref() == Some("communities") {
        format!("/discover/communities?notice={notice}")
    } else {
        format!("/discover?notice={notice}")
    }
}

pub async fn open(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    let Some(entry) = posts::action_entry(&state, &user, id).await? else {
        return Ok(Redirect::to(&action_back(&form, "unavailable")).into_response());
    };
    db::remember_seen(&state.pool, user.current.account.id, &[id], false).await?;
    if let Some(path) = posts::destination(&state, &user, &entry).await? {
        return Ok(Redirect::to(&path).into_response());
    }
    Ok(Redirect::to(&action_back(&form, "unavailable")).into_response())
}

impl PageQuery {
    fn unseen_only(&self) -> bool {
        self.sort.as_deref().unwrap_or("new") == "new" && !self.seen.unwrap_or(false)
    }
    fn options<'a>(
        &'a self,
        kind: &'a str,
        term: &'a str,
        channel: &'a str,
    ) -> Result<db::DiscoveryOptions<'a>, ApiError> {
        let sort = self.sort.as_deref().unwrap_or("new");
        if !["new", "discovered", "published", "active", "surprise"].contains(&sort)
            || self
                .language
                .as_deref()
                .is_some_and(|s| !s.is_empty() && crate::languages::find(s).is_none())
            || self
                .topic
                .as_ref()
                .is_some_and(|s| s.len() > 100 || s.contains('\0'))
        {
            return Err(ApiError::BadRequest("Invalid discovery filters".into()));
        }
        Ok(db::DiscoveryOptions {
            kind,
            sort,
            term,
            channel,
            language: self.language.as_deref().unwrap_or(""),
            topic: self.topic.as_deref().unwrap_or(""),
            source: self.source,
            show_seen: self.seen.unwrap_or(false),
        })
    }
    fn options_query(&self) -> String {
        let mut values = vec![
            ("sort", self.sort.as_deref().unwrap_or("new").to_owned()),
            (
                "language",
                self.language.as_deref().unwrap_or("").to_owned(),
            ),
            ("topic", self.topic.as_deref().unwrap_or("").to_owned()),
            ("seen", self.seen.unwrap_or(false).to_string()),
        ];
        // Source is also present in people/video locations; avoid duplicate keys.
        if let Some(id) = self.source {
            values.push(("source", id.to_string()));
        }
        serde_urlencoded::to_string(values).unwrap_or_default()
    }
}
fn discovery_controls(user: &WebUser, q: &PageQuery, base: &str, kind: &str) -> Markup {
    let l = user.locale;
    html! {
        form.discovery-filter-bar method="get" action=(base) {
            label.discovery-filter-search for="discovery-query" {(l.text("discovery-search"))
                input id="discovery-query" type="search" name="q" maxlength="256" value=(q.q.as_deref().unwrap_or(""));
            }
            @if let Some(channel)=q.channel {input type="hidden" name="channel" value=(channel);}
            label {(l.text("discovery-sort"))
                select name="sort" {
                    @for (value,key) in [("new","discovery-sort-new"),("discovered","discovery-sort-discovered"),("published","discovery-sort-published"),("active","discovery-sort-active"),("surprise","discovery-sort-surprise")] {
                        @if (value!="published" || matches!(kind,"post"|"video")) && (value!="active" || matches!(kind,"profile"|"community")) {
                            option value=(value) selected[q.sort.as_deref().unwrap_or("new")==value] {(l.text(key))}
                        }
                    }
                }
            }
            div.discovery-filter-language {
                span {(l.text("discovery-language"))}
                (view::language_combo_optional("language", &l.text("discovery-language"),
                    &crate::languages::enabled(None), q.language.as_deref().filter(|s| !s.is_empty()),
                    &l.text("discovery-language-all")))
            }
            @if let Some(source)=q.source {input type="hidden" name="source" value=(source);}
            @if q.seen.unwrap_or(false) {input type="hidden" name="seen" value="true";}
            button type="submit" {(l.text("discovery-apply"))}
        }
        @if q.unseen_exhausted {
            p.discovery-state role="status" {
                (l.text(if q.sort.as_deref() == Some("published") {
                    "discovery-caught-up-published"
                } else { "discovery-caught-up-discovered" }))
            }
        }
    }
}
/// Only an initial visit falls back; a continuation must keep its stable journey.
async fn discovery_page(
    state: &AppState,
    user: &WebUser,
    q: &mut PageQuery,
    kind: &str,
    term: &str,
    channel: &str,
) -> Result<browse::Page, ApiError> {
    let cursor = q.cursor()?;
    let mut page = browse::recovery_page(
        state,
        user.current.account.id,
        cursor,
        &q.options(kind, term, channel)?,
    )
    .await?;
    if cursor.is_none() && q.unseen_only() && page.entries.is_empty() && page.next.is_none() {
        q.unseen_exhausted = true;
        q.sort = Some(
            if matches!(kind, "post" | "video") {
                "published"
            } else {
                "discovered"
            }
            .into(),
        );
        page = browse::recovery_page(
            state,
            user.current.account.id,
            None,
            &q.options(kind, term, channel)?,
        )
        .await?;
    }
    Ok(page)
}

/// With scripts disabled the loaded page, rather than an unobservable viewport,
/// is the unit of reading. A one-use, account-bound receipt avoids GET rendering
/// marking JS-prefetched cards seen, and never exposes a session/CSRF token.
fn is_append(headers: &HeaderMap) -> bool {
    headers
        .get("X-Requested-With")
        .is_some_and(|v| v == "fetch")
}
async fn page_receipt(
    state: &AppState,
    user: &WebUser,
    entries: &[db::Entry],
    record_page: bool,
) -> Result<Markup, ApiError> {
    // DOMParser parses fetched HTML with scripting disabled: omit receipts
    // there so parsing an automatic next page cannot mark its cards seen.
    if !record_page || entries.is_empty() {
        return Ok(html! {});
    }
    let token = crate::auth::generate_secret();
    db::create_page_receipt(
        &state.pool,
        user.current.account.id,
        &crate::auth::hash_secret(&token),
        &entries.iter().map(|e| e.id).collect::<Vec<_>>(),
    )
    .await?;
    Ok(
        html! {noscript {img.discovery-page-receipt src=(format!("/web/discover/page/{token}")) alt="" width="1" height="1";}},
    )
}
pub async fn page_seen(
    State(state): State<AppState>,
    user: WebUser,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    if token.len() <= 128 {
        db::consume_page_receipt(
            &state.pool,
            user.current.account.id,
            &crate::auth::hash_secret(&token),
        )
        .await?;
    }
    // A transparent GIF, delivered only to the signed-in owner of the receipt.
    Ok(([("Content-Type", "image/gif"), ("Cache-Control", "private, no-store")],
        &b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;"[..]).into_response())
}
#[derive(Deserialize)]
pub struct SeenForm {
    csrf: String,
    ids: Option<String>,
    back: Option<String>,
}
pub async fn seen(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<SeenForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    let ids: Vec<i64> = form
        .ids
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.parse().ok())
        .take(51)
        .collect();
    db::remember_seen(&state.pool, user.current.account.id, &ids, true).await?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}
pub async fn reset_history(
    State(state): State<AppState>,
    user: WebUser,
    Form(form): Form<SeenForm>,
) -> Result<Response, ApiError> {
    if !user.csrf_ok(&form.csrf) {
        return Ok(csrf_rejection());
    }
    db::reset_seen(&state.pool, user.current.account.id).await?;
    let back = form
        .back
        .as_deref()
        .filter(|s| {
            [
                "/discover",
                "/discover/posts",
                "/discover/videos",
                "/discover/communities",
            ]
            .contains(s)
        })
        .unwrap_or("/discover");
    Ok(Redirect::to(back).into_response())
}

pub async fn server_icon(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let source = db::source(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let domains = account_domain_block::all_domains(&state.pool, user.current.account.id).await?;
    if !browse::source_visible(&state, &source, &domains).await? {
        return Err(ApiError::NotFound);
    }
    let bytes = db::server_icon(&state.pool, id)
        .await?
        .and_then(|i| i.image)
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            ("Content-Type", "image/png"),
            ("Cache-Control", "private, no-cache"),
        ],
        bytes,
    )
        .into_response())
}

/// Normal search shares the same cards, eligibility and open action as Discover.
pub(super) async fn search_cards(
    state: &AppState,
    user: &WebUser,
    entries: &[db::Entry],
    term: &str,
) -> Result<Markup, ApiError> {
    let back = format!(
        "/search?{}",
        serde_urlencoded::to_string([("q", term)]).unwrap_or_default()
    );
    let images = images::Images::load(state, user, entries, &[]).await?;
    let mut cards = Vec::new();
    for entry in entries {
        cards.push(match entry.kind.as_str() {
            "post" => posts::card(state, entry, user, term, &back, &images).await?,
            "video" => videos::card(state, entry, user, &back, &images).await?,
            _ => card(entry, user, &back, &images),
        });
    }
    Ok(html! {div.discovery-grid {@for card in cards {(card)}}})
}
