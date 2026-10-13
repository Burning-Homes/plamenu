//! Format preparation and cache management inside the shared Music workspace.
use super::{
    i18n::Locale,
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{AppState, error::ApiError, music_renditions};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct Options {
    job: Option<i64>,
    token: Option<String>,
}
fn response(title: &str, user: Option<&WebUser>, body: &maud::Markup, shared: bool) -> Response {
    let mut response = (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(title, user, body),
    )
        .into_response();
    if shared {
        response
            .headers_mut()
            .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
        response
            .headers_mut()
            .insert("x-robots-tag", "noindex, noarchive".parse().unwrap());
    }
    response
}
pub async fn page(
    State(state): State<AppState>,
    Path(asset): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let scope = if let Some(raw) = &options.token {
        Some(crate::music_shares::resolve(&state, raw, viewer).await?)
    } else {
        None
    };
    let source = if let Some(scope) = &scope {
        crate::music_shares::asset(&state, scope, asset, viewer).await?
    } else {
        crate::music_assets::get(&state, asset, viewer).await?
    };
    let rows = if let Some(scope) = &scope {
        music_renditions::list_shared(&state, asset, viewer, scope).await?
    } else {
        music_renditions::list(&state, asset, viewer).await?
    };
    let query = options
        .token
        .as_ref()
        .map_or_else(String::new, |token| format!("&token={token}"));
    let back = options.token.as_ref().map_or_else(
        || format!("/music/assets/{asset}"),
        |token| format!("/music/share?token={token}"),
    );
    let selected = if let Some(job) = options.job {
        let row = if let Some(scope) = &scope {
            music_renditions::get_shared(&state, asset, job, viewer, scope).await?
        } else {
            music_renditions::get(&state, asset, job, viewer).await?
        };
        music_renditions::authorize_playback(&state, &row, viewer).await?;
        Some(row)
    } else {
        None
    };
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |u| u.locale);
    let title = l.text("music-renditions-title");
    let body = html! {section.column.music-library{
        h1{(title) ": " (source.title)}
        (super::music::workspace_navigation(l,user.is_some(),"tracks"))
        p{a href=(back){(l.text("music-renditions-back"))}}
        @if let Some(row)=selected{
            section data-rendition-state=(row.state){
                h2{(row.format.to_uppercase())}
                p{(l.text(&format!("music-rendition-{}",row.state)))}
                @if row.state=="ready"{
                    @if row.format=="flac"{p data-actual-bitrate="lossless"{(l.text("music-renditions-lossless"))}}@else if let Some(rate)=row.actual_bitrate{p data-actual-bitrate=(rate){(l.text("music-renditions-bitrate")) ": " (rate)}}
                    audio controls preload="metadata" aria-label=(source.title) src=(options.token.as_ref().map_or_else(||format!("/music/assets/{asset}/renditions/{}/stream",row.id),|token|format!("/music/share/assets/{asset}/renditions/{}/stream?token={token}",row.id))){}
                }@else{
                    @if let Some(error)=row.error{p{(error)}}
                    p{a href=(format!("/music/assets/{asset}/renditions?job={}{query}",row.id)){(l.text("music-renditions-refresh"))}}
                }
            }
        }
        @if user.is_some() || scope.is_some(){
            p{(l.text("music-renditions-hint"))}
            form.settings-form method="post" action=(format!("/web/music/assets/{asset}/renditions")){
                @if let Some(user)=&user{input type="hidden" name="csrf" value=(user.csrf);}
                @if let Some(token)=&options.token{input type="hidden" name="token" value=(token);}
                label for="music-rendition-format"{(l.text("music-renditions-format"))}
                select id="music-rendition-format" name="format"{
                    @for (format,label) in [("mp3","MP3"),("ogg","Ogg Vorbis"),("flac","FLAC"),("aac","AAC"),("opus","Opus")]{option value=(format){(label)}}
                }
                label for="music-rendition-bitrate"{(l.text("music-renditions-bitrate"))}
                input id="music-rendition-bitrate" name="bitrate" type="number" min="32" max="512" value=(state.config.music.default_bitrate) required;
                button type="submit"{(l.text("music-renditions-prepare"))}
                button type="submit" name="play_now" value="1"{(l.text("music-renditions-play-now"))}
            }
        }
        h2{(l.text("music-renditions-cached"))}
        @if rows.is_empty(){p{(l.text("music-renditions-empty"))}}
        ul{@for row in rows{li data-rendition-format=(row.format){
            (row.format.to_uppercase()) " · " @if row.format=="flac"{(l.text("music-renditions-lossless"))}@else{(row.actual_bitrate.unwrap_or(row.bitrate)) " kbps"} " · " (l.text(&format!("music-rendition-{}",row.state)))
            " " a href=(format!("/music/assets/{asset}/renditions?job={}{query}",row.id)){(l.text(if row.state=="ready"{"music-renditions-listen"}else{"music-renditions-check"}))}
            @if let Some(user)=&user{@if source.owner_id==user.current.account.id{
                form method="post" action=(format!("/web/music/assets/{asset}/renditions/{}/remove",row.id)){
                    input type="hidden" name="csrf" value=(user.csrf);
                    button type="submit"{(l.text(if row.state=="ready"{"music-renditions-remove"}else{"music-renditions-cancel"}))}
                }
            }}
        }}}
    }};
    Ok(response(&title, user.as_ref(), &body, scope.is_some()))
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let fields = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(fields)
}
pub async fn prepare(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(asset): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = if let Some(user) = &user {
        form(user, &body)?
    } else {
        settings::form_pairs(&body).map_err(ApiError::Unprocessable)?
    };
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let token = settings::field(&fields, "token");
    if user.is_none() && token.is_none() {
        return Err(ApiError::Unauthorized(
            "Sign in or open a shared track".into(),
        ));
    }
    let format = settings::field(&fields, "format").unwrap_or_default();
    let bitrate = settings::field(&fields, "bitrate")
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| ApiError::Unprocessable("Invalid bitrate".into()))?;
    let selection = music_renditions::negotiate(&state, viewer, format, bitrate).await?;
    if settings::field(&fields, "play_now") == Some("1") {
        if let Some(token) = token {
            let scope = crate::music_shares::resolve(&state, token, viewer).await?;
            crate::music_shares::asset(&state, &scope, asset, viewer).await?;
        } else {
            crate::music_assets::get(&state, asset, viewer).await?;
        }
        let token_query = token.map_or_else(String::new, |token| format!("&token={token}"));
        return Ok(settings::redirect_to(&format!(
            "/music/assets/{asset}/renditions/play?format={}&bitrate={}{token_query}",
            selection.format.name(),
            selection.bitrate
        )));
    }
    let row = if let Some(token) = token {
        let scope = crate::music_shares::resolve(&state, token, viewer).await?;
        music_renditions::request_shared(&state, asset, viewer, selection, &scope).await?
    } else {
        music_renditions::request(&state, asset, viewer, selection).await?
    };
    let query = token.map_or_else(String::new, |token| format!("&token={token}"));
    Ok(settings::redirect_to(&format!(
        "/music/assets/{asset}/renditions?job={}{query}",
        row.id
    )))
}
#[derive(Deserialize)]
pub struct PlayOptions {
    format: String,
    bitrate: Option<u32>,
    token: Option<String>,
}
pub async fn play(
    State(state): State<AppState>,
    Path(asset): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    Query(options): Query<PlayOptions>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let source = if let Some(token) = &options.token {
        let scope = crate::music_shares::resolve(&state, token, viewer).await?;
        crate::music_shares::asset(&state, &scope, asset, viewer).await?
    } else {
        crate::music_assets::get(&state, asset, viewer).await?
    };
    let selection =
        music_renditions::negotiate(&state, viewer, &options.format, options.bitrate).await?;
    let query = format!(
        "format={}&bitrate={}",
        selection.format.name(),
        selection.bitrate
    );
    let (stream, back) = options.token.as_ref().map_or_else(
        || {
            (
                format!("/music/assets/{asset}/stream?{query}"),
                format!("/music/assets/{asset}/renditions"),
            )
        },
        |token| {
            (
                format!("/music/share/assets/{asset}/stream?token={token}&{query}"),
                format!("/music/assets/{asset}/renditions?token={token}"),
            )
        },
    );
    let l = user
        .as_ref()
        .map_or_else(|| Locale::from_headers(&headers), |u| u.locale);
    let title = l.text("music-renditions-play-now");
    let body = html! {section.column.music-library{
        h1{(source.title)}(super::music::workspace_navigation(l,user.is_some(),"tracks"))
        p{(l.text("music-renditions-stream-hint"))}
        p{(selection.format.name().to_uppercase()) " · " @if selection.format==music_renditions::Format::Flac{(l.text("music-renditions-lossless"))}@else{span data-bitrate-limit=(selection.bitrate){(l.text("music-renditions-ceiling")) ": " (selection.bitrate) " kbps"}}}
        audio controls preload="none" aria-label=(source.title) src=(stream){}
        p{a href=(back){(l.text("music-renditions-title"))}}
    }};
    Ok(response(
        &title,
        user.as_ref(),
        &body,
        options.token.is_some(),
    ))
}
pub async fn remove(
    State(state): State<AppState>,
    user: WebUser,
    Path((asset, job)): Path<(i64, i64)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    form(&user, &body)?;
    music_renditions::cancel(&state, asset, job, user.current.account.id).await?;
    Ok(settings::redirect_to(&format!(
        "/music/assets/{asset}/renditions"
    )))
}
