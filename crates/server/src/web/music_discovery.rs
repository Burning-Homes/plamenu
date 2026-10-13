//! The ordinary listening entry point accepts a server or resource link.
use super::{layout, session::WebUser, settings};
use crate::{AppState, error::ApiError, music_remote};
use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
#[derive(Default, serde::Deserialize)]
pub struct Request {
    pub url: Option<String>,
    pub server: Option<String>,
    pub kind: Option<String>,
    pub q: Option<String>,
    pub offset: Option<usize>,
}
fn link(path: &str, pairs: &[(&str, &str)]) -> String {
    format!(
        "{path}?{}",
        serde_urlencoded::to_string(pairs).unwrap_or_default()
    )
}
pub(super) fn lookup(user: &WebUser, value: &str) -> maud::Markup {
    let l = user.locale;
    html! {form.settings-form.music-discovery method="get" action="/music/open" {
        label for="music-open-url"{(l.text("music-open-label"))}
        input id="music-open-url" name="url" type="url" required maxlength="2048" value=(value) placeholder="https://music.example/";
        p{(l.text("music-open-hint"))}
        button type="submit"{(l.text("music-open-submit"))}
    }}
}
pub async fn open(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<Request>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let input = q.url.as_deref().unwrap_or_default().trim();
    let result = async {
        let url = music_remote::document::url(input)?;
        if ["/tracks", "/albums", "/artists", "/streams", "/radios"]
            .contains(&url.path().trim_end_matches('/'))
        {
            return Ok(settings::redirect_to(&link(
                "/music/discover",
                &[
                    ("server", &url.origin().ascii_serialization()),
                    ("kind", url.path().trim_matches('/')),
                ],
            )));
        }
        if matches!(url.path(), "" | "/" | "/ap/actor") {
            return Ok(settings::redirect_to(&link(
                "/music/discover",
                &[("server", input)],
            )));
        }
        let resource = match music_remote::import(&state, user.current.account.id, input).await {
            Err(ApiError::NotFound)
                if url
                    .path()
                    .strip_prefix("/tracks/")
                    .is_some_and(crate::audio::valid_mbid) =>
            {
                music_remote::catalogue::import_track(
                    &state,
                    user.current.account.id,
                    &format!(
                        "{}/api/v1{}",
                        url.origin().ascii_serialization(),
                        url.path()
                    ),
                )
                .await?
            }
            result => result?,
        };
        Ok(settings::redirect_to(&format!(
            "/music/remote/{}",
            resource.id
        )))
    }
    .await;
    match result {
        Ok(response) => Ok(response),
        Err(error) => problem(&user, input, error),
    }
}
fn problem(user: &WebUser, input: &str, error: ApiError) -> Result<Response, ApiError> {
    let message = match error {
        ApiError::NotFound => user.locale.text("music-open-unavailable"),
        ApiError::Unprocessable(message) | ApiError::BadRequest(message) => message,
        other => return Err(other),
    };
    let l = user.locale;
    let body = html! {section.column {h1{(l.text("music-title"))}p.settings__error role="alert"{(message)}(lookup(user,input))a href="/music"{(l.text("music-title"))}}};
    Ok((
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        [(header::CACHE_CONTROL, "private, no-store")],
        layout::shell(&l.text("music-title"), Some(user), &body),
    )
        .into_response())
}
pub async fn browse(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<Request>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let server = q.server.as_deref().unwrap_or_default();
    let kind = q.kind.as_deref().unwrap_or("tracks");
    let term = q.q.as_deref().unwrap_or_default();
    let offset = q.offset.unwrap_or_default();
    let l = user.locale;
    let result = music_remote::catalogue::browse(
        &state,
        user.current.account.id,
        server,
        kind,
        term,
        offset,
    )
    .await;
    let page = match result {
        Ok(page) => page,
        Err(error) => return problem(&user, server, error),
    };
    let body = html! {section.column {
        h1{(page.title)}(super::music::workspace_navigation(l,true,"public"))
        p{a href="/music"{(l.text("music-discovery-back"))}}
        nav.music-primary aria-label=(l.text("music-discovery-types")) {@for (value,key) in [("tracks","music-mine-tracks"),("albums","music-albums-title"),("artists","music-artists-title"),("streams","music-live-radio")] {
            a href=(link("/music/discover",&[("server",server),("kind",value),("q",term)])) aria-current=(if kind==value {"page"}else{"false"}){(l.text(key))}
        }}
        form.settings-form method="get" action="/music/discover" {
            input type="hidden" name="server" value=(server);input type="hidden" name="kind" value=(kind);
            label for="music-discovery-search"{(l.text("music-search"))}input id="music-discovery-search" name="q" type="search" maxlength="200" value=(term);
            button type="submit"{(l.text("music-search-submit"))}
        }
        @if page.items.is_empty(){p{(l.text("music-discovery-empty"))}}
        ul.music-discovery-results {@for item in &page.items {li {
            h2{a href=(link("/music/open",&[("url",&item.url)])){(item.name)}}
            @if !item.subtitle.is_empty(){p{(item.subtitle)}}
            @if item.playable {(super::music_player::url_play_form(&user,&item.url))}
            @else if kind=="streams" {p{(l.text("music-discovery-offline"))}}
            @else if kind=="radios" {p{(l.text("music-discovery-radio-empty"))}}
        }}}
        details{summary{(l.text("music-resource-tools"))}a href=(link("/music/discover",&[("server",server),("kind","radios")])){(l.text("music-radio-rules"))}p{(l.text("music-radio-rules-hint"))}}
        @if page.more {a rel="next" href=(link("/music/discover",&[("server",server),("kind",kind),("q",term),("offset",&(offset+music_remote::catalogue::PAGE_SIZE).to_string())])){(l.text("music-older"))}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&page.title, Some(&user), &body),
    )
        .into_response())
}
