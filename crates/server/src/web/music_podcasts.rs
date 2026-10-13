//! Native podcast forms and episode playback within the Music workspace.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_podcasts::{self, Query as CatalogueQuery},
};
use axum::{
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
fn response(title: &str, user: &WebUser, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        layout::shell(title, Some(user), body),
    )
        .into_response()
}
fn description(raw: &str) -> Markup {
    html! {div.status__content{(maud::PreEscaped(plamenu_ap::text::sanitize_remote_html(raw)))}}
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<CatalogueQuery>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let page = music_podcasts::list(&state, user.current.account.id, &query).await?;
    let l = user.locale;
    let title = l.text("music-podcasts-title");
    let q = query.q.as_deref().unwrap_or_default();
    let sort = query.sort.as_deref().unwrap_or("latest");
    let body = html! {section.column.music-library{
        h1{(title)}
        (super::music::workspace_navigation(l,true,"podcasts"))
        p{(l.text("music-podcasts-hint"))}
        @if !page.items.is_empty() || !q.is_empty() {
        form.settings-form.music-podcast-search method="get" action="/music/podcasts"{
            label for="podcast-search"{(l.text("music-podcasts-search"))}input id="podcast-search" type="search" name="q" maxlength="200" value=(q);
            details{summary{(l.text("music-podcasts-sort"))}
            label for="podcast-sort"{(l.text("music-podcasts-sort"))}select id="podcast-sort" name="sort"{@for(value,key)in[("latest","music-podcasts-latest"),("episodes","music-podcasts-count"),("unplayed","music-podcasts-unplayed"),("name","music-podcasts-name")]{option value=(value) selected[value==sort]{(l.text(key))}}}
            label for="podcast-direction"{(l.text("music-podcasts-direction"))}select id="podcast-direction" name="direction"{@for(value,key)in[("desc","music-podcasts-desc"),("asc","music-podcasts-asc")]{option value=(value) selected[query.direction.as_deref().unwrap_or(if sort=="name"{"asc"}else{"desc"})==value]{(l.text(key))}}}
            }
            button type="submit"{(l.text("music-search-submit"))}
        }
        }
        details open[page.items.is_empty()]{summary{(l.text("music-podcasts-subscribe"))}form.settings-form method="post" action="/web/music/podcasts/subscribe"{input type="hidden" name="csrf" value=(user.csrf);label for="podcast-feed"{(l.text("music-podcasts-feed"))}input id="podcast-feed" type="url" name="feed_url" required maxlength="2048";button type="submit"{(l.text("music-podcasts-subscribe"))}}}
        details{summary{(l.text("music-podcast-tools"))}
        details{summary{(l.text("music-podcasts-opml"))}p{a href="/music/podcasts/opml"{(l.text("music-podcasts-export"))}}form.settings-form method="post" action="/web/music/podcasts/import"{input type="hidden" name="csrf" value=(user.csrf);label for="podcast-opml"{(l.text("music-podcasts-import-hint"))}textarea id="podcast-opml" name="opml" maxlength="2097152" required {}button type="submit"{(l.text("music-podcasts-import"))}}}
        form.settings-form method="post" action="/web/music/podcasts/import-file" enctype="multipart/form-data"{
            input type="hidden" name="csrf" value=(user.csrf);label for="podcast-opml-file"{(l.text("music-podcasts-import-file"))}input id="podcast-opml-file" type="file" name="file" accept=".opml,.xml,text/x-opml,application/xml,text/xml" required;button type="submit"{(l.text("music-podcasts-import"))}
        }
        }
        @if page.items.is_empty(){p{(l.text("music-podcasts-empty"))}}
        ul{@for show in &page.items{li{@if show["image_url"].is_string(){img.music-artwork loading="lazy" width="128" height="128" alt="" src=(format!("/music/podcasts/shows/{}/image",show["id"].as_str().unwrap_or_default()));}h2{a href=(show["url"].as_str().unwrap_or_default()){(show["title"].as_str().unwrap_or_default())}}p{(show["author"].as_str().unwrap_or_default())}p{(l.text("music-podcasts-count"))": "(show["episode_count"].as_i64().unwrap_or(0))" · "(l.text("music-podcasts-unplayed"))": "(show["unplayed_count"].as_i64().unwrap_or(0))}@if let Some(error)=show["last_error"].as_str(){p.settings__error role="status"{(error)}}}}}
        @if let Some(next)=page.next{a href=(format!("/music/podcasts?{}",serde_urlencoded::to_string([("q",q.to_owned()),("sort",sort.to_owned()),("direction",query.direction.clone().unwrap_or_else(||if sort=="name"{"asc".into()}else{"desc".into()})),("offset",next.to_string())]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(&title, &user, &body))
}
#[derive(Default, Deserialize)]
pub struct Episodes {
    sort: Option<String>,
    offset: Option<i64>,
}
pub async fn show(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<Episodes>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let show = music_podcasts::get(&state, id, user.current.account.id).await?;
    let following = music_podcasts::following(&state, id, user.current.account.id).await?;
    let oldest = match query.sort.as_deref() {
        None | Some("newest") => false,
        Some("oldest") => true,
        _ => return Err(ApiError::Unprocessable("Invalid episode order".into())),
    };
    let episodes =
        music_podcasts::episodes(&state, id, user.current.account.id, oldest, query.offset).await?;
    let collections: Vec<crate::music_collections::Collection> = sqlx::query_as(
        "SELECT * FROM music_collections WHERE owner_id=$1 ORDER BY lower(name),id LIMIT 100",
    )
    .bind(user.current.account.id)
    .fetch_all(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let l = user.locale;
    let clock = super::pages::clock_for(Some(&user), l);
    let body = html! {section.column.music-library{h1{(show.title)}(super::music::workspace_navigation(l,true,"podcasts"))
        @if show.image_url.is_some(){img.music-artwork width="256" height="256" alt="" src=(format!("/music/podcasts/shows/{id}/image"));}
        @if !show.author.is_empty(){p{(show.author)}}@if !show.language.is_empty(){p{(show.language)}}@if !show.categories.is_empty(){p{(show.categories.join(" · "))}}@if show.explicit{p{(l.text("music-podcasts-explicit"))}}(description(&show.description))
        @if let Some(link)=&show.link{p{a href=(link) rel="noreferrer nofollow"{(l.text("music-podcasts-website"))}}}
        p{a href=(show.feed_url) rel="noreferrer nofollow"{(l.text("music-podcasts-feed"))}}
        @if let Some(error)=&show.last_error{p.settings__error role="status"{(error)}}
        @if let Some(date)=show.last_fetched_at{p{(l.text("music-podcasts-refreshed"))" "(clock.element_absolute_iso(&crate::entities::rfc3339(date)?))}}
        form method="post" action=(if following{format!("/web/music/podcasts/{id}/unsubscribe")}else{"/web/music/podcasts/subscribe".into()}){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="feed_url" value=(show.feed_url);button type="submit"{(l.text(if following{"music-podcasts-unsubscribe"}else{"music-podcasts-subscribe"}))}}
        h2{(l.text("music-podcasts-episodes"))}
        @if episodes.items.is_empty(){p{(l.text("music-podcasts-no-episodes"))}}
        ol{@for episode in &episodes.items{li{h2{a href=(episode["url"].as_str().unwrap_or_default()){(episode["title"].as_str().unwrap_or_default())}}@if let Some(id)=episode["id"].as_str().and_then(|id|id.parse::<i64>().ok()){(super::music_player::play_form(&user,"episode",id))}@if let Some(date)=episode["published_at"].as_str(){p{(clock.element_absolute_iso(date))}}@if episode["played"].as_bool()==Some(true){p{(l.text("music-podcasts-played"))}}details{summary{(l.text("music-podcasts-episode-tools"))}label{input type="checkbox" name="episode" form="podcast-bulk" value=(episode["id"].as_str().unwrap_or_default()) checked;(l.text("music-podcasts-select-episode"))}}}}}
        details{summary{(l.text("music-podcasts-tools"))}
        form method="post" action=(format!("/web/music/podcasts/{id}/refresh")){input type="hidden" name="csrf" value=(user.csrf);button type="submit"{(l.text("music-podcasts-refresh"))}}
        form.settings-form method="get" action=(format!("/music/podcasts/{id}")){label for="episode-order"{(l.text("music-podcasts-direction"))}select id="episode-order" name="sort"{option value="newest" selected[!oldest]{(l.text("music-podcasts-newest"))}option value="oldest" selected[oldest]{(l.text("music-podcasts-oldest"))}}button type="submit"{(l.text("music-search-submit"))}}
        @if !episodes.items.is_empty(){
            p{a.music-player-open href=(format!("/music/player?podcast_id={id}&offset={}&oldest={oldest}",query.offset.unwrap_or(0))) target="plamenu-music-player"{(l.text("music-podcasts-page-queue"))}}
            form.settings-form id="podcast-bulk" method="post" action=(format!("/web/music/podcasts/{id}/played")){
                input type="hidden" name="csrf" value=(user.csrf);
                @if !collections.is_empty(){label for="podcast-bulk-collection"{(l.text("music-podcasts-collection"))}select id="podcast-bulk-collection" name="collection"{@for collection in &collections{option value=(format!("{}:{}",collection.id,collection.revision)){(collection.name)}}}button type="submit" formaction=(format!("/web/music/podcasts/{id}/collect")){(l.text("music-podcasts-selected-collection"))}}
                button type="submit" name="enabled" value="true"{(l.text("music-podcasts-page-played"))}
                button type="submit" name="enabled" value="false"{(l.text("music-podcasts-page-unplayed"))}
            }
        }
        }
        @if let Some(next)=episodes.next{a href=(format!("/music/podcasts/{id}?sort={}&offset={next}",if oldest{"oldest"}else{"newest"})){(l.text("music-older"))}}
    }};
    Ok(response(&show.title, &user, &body))
}
pub async fn episode(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let episode = music_podcasts::episode(&state, id, user.current.account.id).await?;
    let show = music_podcasts::get(&state, episode.podcast_id, user.current.account.id).await?;
    let collections: Vec<crate::music_collections::Collection> = sqlx::query_as(
        "SELECT * FROM music_collections WHERE owner_id=$1 ORDER BY lower(name),id LIMIT 100",
    )
    .bind(user.current.account.id)
    .fetch_all(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let l = user.locale;
    let clock = super::pages::clock_for(Some(&user), l);
    let body = html! {section.column.music-library{h1{(episode.title)}(super::music::workspace_navigation(l,true,"podcasts"))p{a href=(format!("/music/podcasts/{}",show.id)){(show.title)}}@if let Some(date)=episode.published_at{p{(clock.element_absolute_iso(&crate::entities::rfc3339(date)?))}}
        @if episode.image_url.is_some() || show.image_url.is_some(){img.music-artwork width="256" height="256" alt="" src=(format!("/music/podcasts/episodes/{id}/image"));}
        details{summary{(l.text("music-resource-tools"))}
        (super::music_downloads::link(&user,"episode",id))
        p{a.music-player-open href=(format!("/music/player?episode_id={id}")) target="plamenu-music-player"{(l.text("music-player-add"))}}
        @if !collections.is_empty(){form.settings-form method="post" action=(format!("/web/music/podcasts/episodes/{id}/collect")){
            input type="hidden" name="csrf" value=(user.csrf);
            label for="podcast-collection"{(l.text("music-podcasts-collection"))}select id="podcast-collection" name="collection" required{@for collection in &collections{option value=(collection.id){(collection.name)}}}
            button type="submit"{(l.text("music-podcasts-add-collection"))}
        }}
        }
        audio controls preload="none" data-podcast-episode=(id) data-csrf=(user.csrf) src=(format!("/music/podcasts/episodes/{id}/audio")) aria-label=(episode.title) {}
        (description(&episode.description))
        p{a href=(episode.audio_url) rel="noreferrer nofollow"{(l.text("music-podcasts-source"))}}
        @if let Some(link)=&episode.link{p{a href=(link) rel="noreferrer nofollow"{(l.text("music-podcasts-website"))}}}
        p.podcast-played-state aria-live="polite" data-played=(l.text("music-podcasts-played")) data-unplayed=(l.text("music-podcasts-not-played")){(l.text(if episode.played{"music-podcasts-played"}else{"music-podcasts-not-played"}))}
        form.podcast-played-toggle method="post" action=(format!("/web/music/podcasts/episodes/{id}/played")){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="enabled" value=(if episode.played{"false"}else{"true"});button type="submit" data-played=(l.text("music-podcasts-mark-unplayed")) data-unplayed=(l.text("music-podcasts-mark-played")){(l.text(if episode.played{"music-podcasts-mark-unplayed"}else{"music-podcasts-mark-played"}))}}
    }};
    Ok(response(&episode.title, &user, &body))
}
fn fields(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:statuses")?;
    let fields = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(fields)
}
fn error_page(user: &WebUser, error: ApiError, back: &str) -> Result<Response, ApiError> {
    let message = match error {
        ApiError::Unprocessable(message) | ApiError::Conflict(message) => message,
        error => return Err(error),
    };
    let l = user.locale;
    let title = l.text("music-podcasts-title");
    let body = html! {section.column{h1{(title)}p.settings__error role="alert"{(message)}p{a href=(back){(l.text("music-podcasts-title"))}}}};
    let mut r = response(&title, user, &body);
    *r.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
    Ok(r)
}
pub async fn subscribe(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    match music_podcasts::subscribe(
        &state,
        user.current.account.id,
        settings::field(&fields, "feed_url").unwrap_or_default(),
    )
    .await
    {
        Ok((show, _)) => Ok(settings::redirect_to(&format!(
            "/music/podcasts/{}",
            show.id
        ))),
        Err(error) => error_page(&user, error, "/music/podcasts"),
    }
}
pub async fn unsubscribe(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    fields(&user, &body)?;
    music_podcasts::unsubscribe(&state, user.current.account.id, id).await?;
    Ok(settings::redirect_to("/music/podcasts"))
}
pub async fn refresh(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    fields(&user, &body)?;
    let back = format!("/music/podcasts/{id}");
    match music_podcasts::refresh(&state, id, user.current.account.id).await {
        Ok(_) => Ok(settings::redirect_to(&back)),
        Err(error) => error_page(&user, error, &back),
    }
}
pub async fn played(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let enabled = match settings::field(&fields, "enabled") {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(ApiError::Unprocessable("Invalid played state".into())),
    };
    music_podcasts::played(&state, user.current.account.id, id, enabled).await?;
    Ok(settings::redirect_to(&format!(
        "/music/podcasts/episodes/{id}"
    )))
}
pub async fn opml(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/x-opml; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"podcasts.opml\"",
            ),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        music_podcasts::opml(&state, user.current.account.id).await?,
    )
        .into_response())
}
pub async fn import(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let result = match music_podcasts::import_opml(
        &state,
        user.current.account.id,
        settings::field(&fields, "opml")
            .unwrap_or_default()
            .as_bytes(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return error_page(&user, error, "/music/podcasts"),
    };
    Ok(import_result(&user, &result))
}
fn import_result(user: &WebUser, result: &serde_json::Value) -> Response {
    let l = user.locale;
    let title = l.text("music-podcasts-import");
    let body = html! {section.column{h1{(title)}p{(l.text("music-podcasts-imported"))": "(result["subscribed"].as_i64().unwrap_or(0))" · "(l.text("music-podcasts-skipped"))": "(result["skipped"].as_i64().unwrap_or(0))" · "(l.text("music-podcasts-failed"))": "(result["failed"].as_i64().unwrap_or(0))}ul{@for error in result["errors"].as_array().into_iter().flatten(){li{(error["feed_url"].as_str().unwrap_or_default())": "(error["error"].as_str().unwrap_or_default())}}}p{a href="/music/podcasts"{(l.text("music-podcasts-title"))}}}};
    response(&title, user, &body)
}

pub async fn collect(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let collection = settings::field(&fields, "collection")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| ApiError::Unprocessable("Choose a music collection".into()))?;
    let current =
        crate::music_collections::get(&state, collection, Some(user.current.account.id)).await?;
    crate::music_collections::add(
        &state,
        user.current.account.id,
        collection,
        &crate::music_collections::Add {
            remote_id: None,
            asset_id: None,
            status_id: None,
            episode_id: Some(id.to_string()),
            revision: current.revision.to_string(),
        },
    )
    .await?;
    Ok(settings::redirect_to(&format!(
        "/music/collections/{collection}"
    )))
}

pub async fn import_file(
    State(state): State<AppState>,
    user: WebUser,
    form: Multipart,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let bytes = crate::routes::music_podcasts::opml_file(form, Some(&user.csrf)).await?;
    match music_podcasts::import_opml(&state, user.current.account.id, &bytes).await {
        Ok(result) => Ok(import_result(&user, &result)),
        Err(error) => error_page(&user, error, "/music/podcasts"),
    }
}

pub async fn played_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(show): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let episodes = fields
        .iter()
        .filter(|(key, _)| key == "episode")
        .map(|(_, value)| value.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    let enabled = match settings::field(&fields, "enabled") {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(ApiError::Unprocessable("Invalid played state".into())),
    };
    if let Err(error) =
        music_podcasts::played_batch(&state, user.current.account.id, show, &episodes, enabled)
            .await
    {
        return error_page(&user, error, &format!("/music/podcasts/{show}"));
    }
    Ok(settings::redirect_to(&format!("/music/podcasts/{show}")))
}

pub async fn collect_page(
    State(state): State<AppState>,
    user: WebUser,
    Path(show): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = fields(&user, &body)?;
    let episodes = fields
        .iter()
        .filter(|(key, _)| key == "episode")
        .map(|(_, value)| value.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    let (collection, revision) = settings::field(&fields, "collection")
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| ApiError::Unprocessable("Choose a collection".into()))?;
    crate::music_collections::add_podcast_episodes(
        &state,
        user.current.account.id,
        collection.parse().map_err(|_| ApiError::NotFound)?,
        revision.parse().map_err(|_| ApiError::NotFound)?,
        show,
        &episodes,
    )
    .await?;
    Ok(settings::redirect_to(&format!(
        "/music/collections/{collection}"
    )))
}
