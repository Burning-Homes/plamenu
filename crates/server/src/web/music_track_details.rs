//! Progressive track notes and album picker; no database IDs are typed by users.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_assets::{self, Asset},
    music_entities::{self, Kind},
    music_track_details,
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;

pub fn description(asset: &Asset) -> Markup {
    let rendered = match asset.description_type.as_str() {
        "text/html" => plamenu_ap::text::sanitize_remote_html(&asset.description),
        "text/markdown" => crate::compose::markdown_to_sanitized_html(&asset.description, false),
        _ => plamenu_ap::text::plain_to_html(&asset.description),
    };
    html! {div.status__content{(maud::PreEscaped(rendered))}}
}
pub fn editor(asset: &Asset, user: &WebUser) -> Markup {
    let l = user.locale;
    html! {
        details{summary{(l.text("music-track-details-edit"))}
            form.settings-form method="post" action=(format!("/web/music/assets/{}/details",asset.id)){
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="revision" value=(asset.revision);
                label for="music-track-description"{(l.text("music-entity-description"))}
                textarea id="music-track-description" name="description" maxlength="50000"{(asset.description)}
                label for="music-track-description-format"{(l.text("music-track-description-format"))}
                select id="music-track-description-format" name="content_type"{
                    @for (value,label) in [("text/plain","music-track-format-plain"),("text/markdown","music-track-format-markdown"),("text/html","music-track-format-html")]{option value=(value) selected[value==asset.description_type]{(l.text(label))}}
                }
                p{(l.text("music-track-description-hint"))}
                label for="music-track-tags"{(l.text("music-entity-tags"))}
                input id="music-track-tags" name="tags" value=(asset.tags.join(", "));
                label for="music-track-credits"{(l.text("music-track-credits"))}
                input id="music-track-credits" name="extra_artists" value=(asset.extra_artists.join(", "));
                p{(l.text("music-track-credits-hint"))}
                button type="submit"{(l.text("music-track-details-save"))}
            }
        }
        p{a href=(format!("/music/assets/{}/album",asset.id)){(l.text("music-track-album-choose"))}}
    }
}
#[derive(Default, Deserialize)]
pub struct Page {
    pub q: Option<String>,
    pub max_id: Option<i64>,
}
pub async fn albums(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let source = music_assets::get(&state, id, Some(user.current.account.id)).await?;
    if source.owner_id != user.current.account.id {
        return Err(ApiError::NotFound);
    }
    let l = user.locale;
    let mut entries = music_entities::catalogue(
        &state,
        Kind::Album,
        Some(user.current.account.id),
        true,
        q.q.as_deref().unwrap_or_default(),
        q.max_id,
    )
    .await?;
    let before = if entries.len() > 60 {
        entries.truncate(60);
        entries.last().map(|e| e.id)
    } else {
        None
    };
    let body = html! {section.column.music-library{
        h1{(l.text("music-track-album-choose"))}
        p{(source.title)} p{(l.text("music-track-album-hint"))}
        form.settings-form method="get" action=(format!("/music/assets/{id}/album")){
            label for="music-album-search"{(l.text("music-search"))}
            input id="music-album-search" type="search" name="q" maxlength="200" value=(q.q.as_deref().unwrap_or_default());
            button type="submit"{(l.text("music-search-submit"))}
        }
        form method="post" action=(format!("/web/music/assets/{id}/album")){
            input type="hidden" name="csrf" value=(user.csrf); input type="hidden" name="revision" value=(source.revision);
            button type="submit"{(l.text("music-track-album-remove"))}
        }
        ul{@for entry in entries{li{
            a href=(format!("/music/albums/{}",entry.id)){(entry.name)}" — "(entry.artist) @if let Some(year)=entry.release_year{" · "(year)}
            form method="post" action=(format!("/web/music/assets/{id}/album")){
                input type="hidden" name="csrf" value=(user.csrf); input type="hidden" name="revision" value=(source.revision);
                input type="hidden" name="album_id" value=(entry.id);
                button type="submit" aria-label=(format!("{}: {} — {}",l.text("music-track-album-select"),entry.name,entry.artist)){(l.text("music-track-album-select"))}
            }
        }}}
        @if let Some(before)=before{a href=(format!("/music/assets/{id}/album?max_id={before}&{}",serde_urlencoded::to_string([("q",q.q.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-older"))}}
        p{a href="/music/albums/new"{(l.text("music-album-create"))}}
        a href=(format!("/music/assets/{id}")){(source.title)}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-track-album-choose"), Some(&user), &body),
    )
        .into_response())
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let form = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    user.current.require_scope("write:media")?;
    Ok(form)
}
pub async fn update(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = form(&user, &body)?;
    let field = |key| settings::field(&form, key).unwrap_or_default().to_owned();
    let terms = |key| {
        field(key)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let input = music_track_details::Details {
        revision: field("revision"),
        description: field("description"),
        content_type: Some(field("content_type")),
        tags: terms("tags"),
        extra_artists: terms("extra_artists"),
    };
    music_track_details::update(&state, user.current.account.id, id, &input).await?;
    Ok(settings::redirect_to(&format!("/music/assets/{id}")))
}
pub async fn set_album(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = form(&user, &body)?;
    let input = music_track_details::Album {
        revision: settings::field(&form, "revision")
            .unwrap_or_default()
            .to_owned(),
        album_id: settings::field(&form, "album_id")
            .filter(|v| !v.is_empty())
            .map(str::to_owned),
    };
    music_track_details::set_album(&state, user.current.account.id, id, &input).await?;
    Ok(settings::redirect_to(&format!("/music/assets/{id}")))
}
