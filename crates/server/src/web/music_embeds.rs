//! Public music widgets never use the visitor's session or sharing capability.
use super::{i18n::Locale, layout, session::MaybeWebUser};
use crate::music_renditions::{Format, Selection};
use crate::{AppState, error::ApiError, music_assets, music_collections, music_entities};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
const MAX_ITEMS: usize = 250;
const WIDGET: &str = include_str!("assets/music-embed.js");
#[derive(Default, Deserialize)]
pub struct PlaybackOptions {
    codec: Option<String>,
    bitrate: Option<u32>,
}
fn playback(
    state: &AppState,
    codec: Option<&str>,
    bitrate: Option<u32>,
) -> Result<Option<Selection>, ApiError> {
    codec
        .filter(|value| !value.is_empty())
        .map(|codec| crate::music_renditions::selection(state.config.music, codec, bitrate, None))
        .transpose()
}
fn playback_query(selection: Option<Selection>) -> String {
    selection.map_or_else(String::new, |selection| {
        format!(
            "?codec={}&bitrate={}",
            selection.format.name(),
            selection.bitrate
        )
    })
}
fn playback_url(item: &music_collections::Playable, selection: Option<Selection>) -> String {
    if let Some(selection) = selection
        && item.stream_url.starts_with("/music/embed/assets/")
    {
        format!(
            "{}?format={}&bitrate={}",
            item.stream_url,
            selection.format.name(),
            selection.bitrate
        )
    } else {
        item.stream_url.clone()
    }
}
#[derive(Clone, Copy)]
enum Kind {
    Track,
    Artist,
    Album,
    Collection,
}
impl Kind {
    fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "assets" => Ok(Self::Track),
            "artists" => Ok(Self::Artist),
            "albums" => Ok(Self::Album),
            "collections" => Ok(Self::Collection),
            _ => Err(ApiError::NotFound),
        }
    }
    fn path(self) -> &'static str {
        match self {
            Self::Track => "assets",
            Self::Artist => "artists",
            Self::Album => "albums",
            Self::Collection => "collections",
        }
    }
}
pub(crate) fn is_frame_path(path: &str) -> bool {
    path.strip_prefix("/music/embed/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(kind, id)| {
            Kind::parse(kind).is_ok() && id.parse::<i64>().is_ok_and(|id| id > 0)
        })
}
struct Catalogue {
    title: String,
    items: Vec<music_collections::Playable>,
    more: bool,
    image_url: Option<String>,
}
fn playable(asset: music_assets::Asset) -> music_collections::Playable {
    music_collections::Playable {
        entry_id: asset.id.to_string(),
        title: asset.title,
        artist: asset.artist,
        url: format!("/music/assets/{}", asset.id),
        stream_url: format!("/music/embed/assets/{}/audio", asset.id),
        duration: asset.duration,
        image_url: asset
            .artwork_id
            .map(|id| format!("/music/embed/artwork/{id}")),
    }
}
async fn catalogue(state: &AppState, kind: Kind, id: i64) -> Result<Catalogue, ApiError> {
    match kind {
        Kind::Track => {
            let asset = music_assets::get(state, id, None).await?;
            Ok(Catalogue {
                title: asset.title.clone(),
                image_url: None,
                items: vec![playable(asset)],
                more: false,
            })
        }
        Kind::Artist | Kind::Album => {
            let entity_kind = if matches!(kind, Kind::Artist) {
                music_entities::Kind::Artist
            } else {
                music_entities::Kind::Album
            };
            let entity = music_entities::get(state, entity_kind, id, None).await?;
            let mut items = Vec::new();
            let mut after = None;
            loop {
                let page = music_entities::tracks(state, entity_kind, id, None, after).await?;
                items.extend(page.items.into_iter().map(playable));
                after = page.after;
                if after.is_none() || items.len() > MAX_ITEMS {
                    break;
                }
            }
            let more = after.is_some() || items.len() > MAX_ITEMS;
            items.truncate(MAX_ITEMS);
            music_entities::get(state, entity_kind, id, None).await?;
            Ok(Catalogue {
                title: entity.name,
                image_url: root_image(
                    state,
                    if matches!(kind, Kind::Artist) {
                        crate::music_artworks::Target::Artists
                    } else {
                        crate::music_artworks::Target::Albums
                    },
                    id,
                )
                .await?,
                items,
                more,
            })
        }
        Kind::Collection => {
            let entity = music_collections::get(state, id, None).await?;
            let mut items = Vec::new();
            let mut after = None;
            loop {
                let (page, next) = music_collections::playable_page(state, id, None, after).await?;
                items.extend(page.into_iter().map(|mut item| {
                    if let Some(id) = item
                        .stream_url
                        .strip_prefix("/music/assets/")
                        .and_then(|tail| tail.strip_suffix("/stream"))
                        .and_then(|id| id.parse::<i64>().ok())
                    {
                        item.stream_url = format!("/music/embed/assets/{id}/audio");
                    }
                    if let Some(image) = &item.image_url
                        && let Some(id) = image
                            .strip_prefix("/music/artwork/")
                            .and_then(|id| id.parse::<i64>().ok())
                    {
                        item.image_url = Some(format!("/music/embed/artwork/{id}"));
                    }
                    item
                }));
                after = next;
                if after.is_none() || items.len() > MAX_ITEMS {
                    break;
                }
            }
            let more = after.is_some() || items.len() > MAX_ITEMS;
            items.truncate(MAX_ITEMS);
            music_collections::get(state, id, None).await?;
            Ok(Catalogue {
                title: entity.name,
                image_url: root_image(state, crate::music_artworks::Target::Collections, id)
                    .await?,
                items,
                more,
            })
        }
    }
}
async fn root_image(
    state: &AppState,
    target: crate::music_artworks::Target,
    id: i64,
) -> Result<Option<String>, ApiError> {
    let rows = crate::music_artworks::for_target(state, target, id, None).await?;
    Ok(rows
        .iter()
        .find(|row| row.slot == "cover")
        .or_else(|| rows.first())
        .map(|row| format!("/music/embed/artwork/{}", row.id)))
}
fn public_response(body: Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response()
}
pub async fn frame(
    State(state): State<AppState>,
    Path((raw, id)): Path<(String, i64)>,
    Query(options): Query<PlaybackOptions>,
    l: Locale,
) -> Response {
    match frame_content(State(state), Path((raw, id)), options, l).await {
        Ok(response) => response,
        Err(ApiError::NotFound) => {
            let title = l.text("music-embed-unavailable");
            let body = html! {section{h1{(title)}p{(l.text("music-embed-unavailable-hint"))}}};
            let mut response = public_response(layout::focused_shell_localized(&title, &body, l));
            *response.status_mut() = axum::http::StatusCode::NOT_FOUND;
            response
        }
        Err(error) => error.into_response(),
    }
}
async fn frame_content(
    State(state): State<AppState>,
    Path((raw, id)): Path<(String, i64)>,
    options: PlaybackOptions,
    l: Locale,
) -> Result<Response, ApiError> {
    let kind = Kind::parse(&raw)?;
    let selection = playback(&state, options.codec.as_deref(), options.bitrate)?;
    let data = catalogue(&state, kind, id).await?;
    let body = html! {section.music-embed.music-library{h1{(data.title)}
        @if let Some(image)=&data.image_url{img.music-artwork src=(image) alt="" width="128" height="128" loading="lazy";}
        @if data.items.is_empty(){p{(l.text("music-empty"))}}
        @if matches!(kind,Kind::Track){@for item in &data.items{(track_card(l,item,false,selection))}}
        @else {ol{@for item in &data.items{li{(track_card(l,item,true,selection))}}}}
        @if data.more{p{(l.text("music-embed-limited"))}}
        p{a href=(format!("/music/{}/{id}",kind.path())) target="_blank" rel="noopener noreferrer"{(l.text("music-embed-open"))}}
        script defer src="/music/embed-frame.js"{}
    }};
    Ok(public_response(layout::focused_shell_localized(
        &data.title,
        &body,
        l,
    )))
}
fn track_card(
    l: Locale,
    item: &music_collections::Playable,
    list: bool,
    selection: Option<Selection>,
) -> Markup {
    html! {
        @if list{h2{(item.title)}}p{(item.artist)}
        @if let Some(image)=&item.image_url{img.music-artwork src=(image) alt="" width="128" height="128" loading="lazy";}
        audio controls preload="none" aria-label=(item.title) src=(playback_url(item,selection)){}
        @if list{p{a href=(item.url) target="_blank" rel="noopener noreferrer"{(l.text("music-embed-open-track"))}}}
    }
}
#[derive(Default, Deserialize)]
pub struct Options {
    format: Option<String>,
    codec: Option<String>,
    bitrate: Option<u32>,
}
fn iframe(
    domain: &str,
    kind: Kind,
    id: i64,
    title: &str,
    requested_height: Option<i64>,
    selection: Option<Selection>,
) -> String {
    let height = requested_height
        .unwrap_or(if matches!(kind, Kind::Track) {
            280
        } else {
            500
        })
        .clamp(1, 1000);
    html!{iframe src=(format!("https://{domain}/music/embed/{}/{id}{}",kind.path(),playback_query(selection))) title=(title) width="100%" height=(height) loading="lazy" referrerpolicy="no-referrer" allow="autoplay"{a href=(format!("https://{domain}/music/{}/{id}",kind.path())){(title)}}}.into_string()
}
fn snippets(
    domain: &str,
    kind: Kind,
    id: i64,
    data: &Catalogue,
    selection: Option<Selection>,
) -> Vec<(&'static str, String)> {
    let page = format!("https://{domain}/music/{}/{id}", kind.path());
    let mut result = Vec::new();
    if matches!(kind, Kind::Track) {
        if let Some(item) = data.items.first() {
            result.push(("audio",html!{p{a href=(page){(data.title)}br;audio src=(absolute(domain,&playback_url(item,selection))) controls preload="none" aria-label=(item.title){}}}.into_string()));
        }
    } else if !data.more && data.items.len() < MAX_ITEMS {
        result.push(("html",html!{div.plamenu-music-list{p{a href=(page){(data.title)}}ol{@for item in &data.items{li{(item.title) " — " (item.artist)br;audio src=(absolute(domain,&playback_url(item,selection))) controls preload="none" aria-label=(item.title){}}}}}}.into_string()));
    }
    let label = data
        .title
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(['\r', '\n'], " ");
    result.push(("markdown", format!("[{label}]({page})")));
    result.push(("script",html!{div data-plamenu-music=(kind.path()) data-id=(id) data-title=(data.title) data-codec=[selection.map(|selection|selection.format.name())] data-bitrate=[selection.map(|selection|selection.bitrate)]{}script async src=(format!("https://{domain}/music/embed.js")){}}.into_string()));
    result.push((
        "iframe",
        iframe(domain, kind, id, &data.title, None, selection),
    ));
    result
}
fn absolute(domain: &str, url: &str) -> String {
    if url.starts_with('/') {
        format!("https://{domain}{url}")
    } else {
        url.to_owned()
    }
}
pub async fn code(
    State(state): State<AppState>,
    Path((raw, id)): Path<(String, i64)>,
    Query(options): Query<Options>,
    headers: HeaderMap,
    MaybeWebUser(user): MaybeWebUser,
) -> Result<Response, ApiError> {
    let kind = Kind::parse(&raw)?;
    let data = catalogue(&state, kind, id).await?;
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |user| user.locale);
    let selection = playback(&state, options.codec.as_deref(), options.bitrate)?;
    let formats = snippets(&state.config.domain, kind, id, &data, selection);
    let selected = options.format.as_deref().unwrap_or(formats[0].0);
    let text = &formats
        .iter()
        .find(|(name, _)| *name == selected)
        .ok_or_else(|| ApiError::Unprocessable("Unavailable music embed format".into()))?
        .1;
    let body = html! {section.column.music-library{h1{(l.text("music-embed-title"))}h2{(data.title)}
        (super::music::workspace_navigation(l,user.is_some(),if matches!(kind,Kind::Collection){"collections"}else if matches!(kind,Kind::Track){"tracks"}else{kind.path()}))
        p{(l.text("music-embed-hint"))}
        form.settings-form method="get"{label for="music-embed-format"{(l.text("music-embed-format"))}select id="music-embed-format" name="format"{@for(name,_)in &formats{option value=(name) selected[*name==selected]{(l.text(&format!("music-embed-format-{name}")))}}}
            label for="music-embed-codec"{(l.text("music-embed-codec"))}select id="music-embed-codec" name="codec"{
                option value="" selected[selection.is_none()]{(l.text("music-embed-codec-default"))}
                @for format in [Format::Mp3,Format::Ogg,Format::Flac,Format::Aac,Format::Opus]{option value=(format.name()) selected[selection.is_some_and(|selected|selected.format==format)]{(format.name().to_uppercase())}}
            }
            label for="music-embed-bitrate"{(l.text("music-renditions-bitrate"))}input id="music-embed-bitrate" name="bitrate" type="number" min="32" max="512" value=(selection.map_or(state.config.music.default_bitrate,|selection|selection.bitrate));
            p{(l.text("music-embed-codec-hint"))}
            button type="submit"{(l.text("music-embed-show"))}}
        label for="music-embed-code"{(l.text("music-embed-code"))}textarea id="music-embed-code" readonly rows="8"{(text)}
        p{a href=(format!("/music/embed/{}/{id}{}",kind.path(),playback_query(selection))){(l.text("music-embed-preview"))}}
        p{a href=(format!("/music/{}/{id}",kind.path())){(l.text("music-embed-open"))}}
    }};
    Ok(public_response(layout::shell_with_metadata(
        &l.text("music-embed-title"),
        user.as_ref(),
        &body,
        &layout::PageMeta::default(),
        l,
    )))
}
pub async fn script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        WIDGET,
    )
        .into_response()
}
pub(super) fn link(l: Locale, kind: &str, id: i64) -> Markup {
    html! {p{a href=(format!("/music/embed-code/{kind}/{id}")){(l.text("music-embed-title"))}}}
}

/// Byte reads use anonymous authorization even when a third-party frame happens
/// to receive a visitor's same-site cookies. Revocation is checked for ranges.
#[derive(Default, Deserialize)]
pub struct AudioOptions {
    format: Option<String>,
    bitrate: Option<u32>,
}
pub async fn audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(options): Query<AudioOptions>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let asset = music_assets::get(&state, id, None).await?;
        let format = options
            .format
            .or_else(|| asset.indexed_original.then(|| "mp3".into()));
        if let Some(format) = &format {
            let access = crate::music_renditions::cold::Access {
                viewer: None,
                scope: None,
            };
            if method == axum::http::Method::HEAD {
                return crate::music_renditions::cold::head(
                    &state,
                    id,
                    access,
                    format,
                    options.bitrate,
                    &headers,
                )
                .await;
            }
            return crate::music_renditions::cold::serve(
                &state,
                id,
                access,
                format,
                options.bitrate,
                &headers,
            )
            .await;
        }
        let asset = music_assets::get(&state, id, None).await?;
        let source = crate::music_bytes::Source::asset(&state, &asset, None, None, false).await?;
        let response = crate::routes::media::stream_stored_file(
            &state,
            &asset.stream_file,
            "audio/mpeg",
            "private, no-store",
            &headers,
        )
        .await?;
        crate::music_bytes::wrap(&state, response, Some(source), None).await
    }
    .await;
    media_response(result.unwrap_or_else(IntoResponse::into_response))
}
pub async fn artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let art = crate::music_artworks::get(&state, id, None).await?;
        let response = crate::routes::media::stream_stored_file(
            &state,
            &art.file_name,
            &art.content_type,
            "private, no-store",
            &headers,
        )
        .await?;
        crate::music_bytes::wrap(
            &state,
            response,
            Some(crate::music_bytes::Source::Artwork {
                id,
                viewer: None,
                scope: None,
                file: art.file_name,
            }),
            None,
        )
        .await
    }
    .await;
    media_response(result.unwrap_or_else(IntoResponse::into_response))
}

pub async fn frame_script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("assets/music-embed-frame.js"),
    )
        .into_response()
}

/// Reuse the existing native oEmbed provider for exact local music page URLs.
/// Parsing never dereferences the supplied URL or accepts a sharing capability.
pub(crate) async fn oembed(
    state: &AppState,
    raw: &str,
    width: i64,
    height: Option<i64>,
) -> Result<Option<serde_json::Value>, ApiError> {
    let Ok(url) = url::Url::parse(raw) else {
        return Ok(None);
    };
    let own = url::Url::parse(&format!("https://{}", state.config.domain))
        .map_err(|_| ApiError::NotFound)?;
    if url.origin() != own.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
    {
        return Ok(None);
    }
    let Some((kind, id)) = url
        .path()
        .strip_prefix("/music/")
        .and_then(|rest| rest.split_once('/'))
    else {
        return Ok(None);
    };
    let Ok(kind) = Kind::parse(kind) else {
        return Ok(None);
    };
    let Some(id) = id.parse::<i64>().ok().filter(|id| *id > 0) else {
        return Ok(None);
    };
    let data = catalogue(state, kind, id).await?;
    let width = width.clamp(1, 800);
    let height = height
        .unwrap_or(if matches!(kind, Kind::Track) {
            280
        } else {
            500
        })
        .clamp(1, 1000);
    Ok(Some(
        serde_json::json!({"version":"1.0","type":"rich","provider_name":"Plamenu", "provider_url":own.as_str(), "title":data.title,"width":width,"height":height,"html":iframe(&state.config.domain,kind,id,&data.title,Some(height),None)}),
    ))
}

pub(super) async fn available(state: &AppState, raw: &str, id: i64) -> Result<bool, ApiError> {
    let result = match Kind::parse(raw)? {
        Kind::Track => music_assets::get(state, id, None).await.map(|_| ()),
        Kind::Artist => music_entities::get(state, music_entities::Kind::Artist, id, None)
            .await
            .map(|_| ()),
        Kind::Album => music_entities::get(state, music_entities::Kind::Album, id, None)
            .await
            .map(|_| ()),
        Kind::Collection => music_collections::get(state, id, None).await.map(|_| ()),
    };
    match result {
        Ok(()) => Ok(true),
        Err(ApiError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn media_response(mut response: Response) -> Response {
    for (key, value) in [
        (header::CACHE_CONTROL, "private, no-store"),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            "Accept-Ranges, Content-Range, Content-Length, Content-Type",
        ),
    ] {
        response.headers_mut().insert(key, value.parse().unwrap());
    }
    response
}
/// Public-media CORS never grants credentials or access to account mutations.
pub async fn media_preflight() -> Response {
    let mut response = media_response(axum::http::StatusCode::NO_CONTENT.into_response());
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, HEAD, OPTIONS".parse().unwrap(),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Range, If-None-Match".parse().unwrap(),
    );
    response
}
