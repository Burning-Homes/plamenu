//! Native music favourites with CSRF forms and current source authorization.
use super::{layout, session::WebUser, settings};
use crate::{AppState, error::ApiError, music_favourites, music_queues::Source};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
#[derive(Default, serde::Deserialize)]
pub struct Page {
    max_id: Option<i64>,
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let page = music_favourites::page(&state, user.current.account.id, page.max_id).await?;
    let l = user.locale;
    let body = html! {section.column.music-favourites {
        h1{(l.text("music-favourites-title"))}
        (super::music::workspace_navigation(l,true,"favourites"))
        p{(l.text("music-favourites-hint"))}
        nav.video-explore__links aria-label=(l.text("music-title")){a href="/music"{(l.text("music-title"))}a.music-player-open href="/music/player" target="plamenu-music-player"{(l.text("music-player-title"))}}
        @if page.items.is_empty(){p{(l.text("music-favourites-empty"))}}
        ul{@for item in &page.items{li{
            a href=(item.track.url){(item.track.title)} " — " (item.track.artist)
            @if let Some(asset)=&item.asset_id{(super::music_player::link(&user,"asset",asset.parse().map_err(|_|ApiError::NotFound)?))}
            @else if let Some(status)=&item.status_id{(super::music_player::link(&user,"status",status.parse().map_err(|_|ApiError::NotFound)?))}
            @if let Some(remote)=&item.remote_id {
                (super::music_player::link(&user,"remote",remote.parse().map_err(|_|ApiError::NotFound)?))
                (remote_form(l,&user.csrf,remote,true,"/music/favourites"))
            } @else {(form(l,&user.csrf,item.asset_id.as_deref(),item.status_id.as_deref(),true,"/music/favourites"))}
        }}}
        @if let Some(before)=&page.before{a href=(format!("/music/favourites?max_id={before}")){(l.text("music-older"))}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-favourites-title"), Some(&user), &body),
    )
        .into_response())
}
pub(super) fn form(
    l: super::i18n::Locale,
    csrf: &str,
    asset: Option<&str>,
    status: Option<&str>,
    saved: bool,
    back: &str,
) -> Markup {
    source_form(l, csrf, asset, status, None, saved, back)
}
pub(super) fn remote_form(
    l: super::i18n::Locale,
    csrf: &str,
    remote: &str,
    saved: bool,
    back: &str,
) -> Markup {
    source_form(l, csrf, None, None, Some(remote), saved, back)
}
fn source_form(
    l: super::i18n::Locale,
    csrf: &str,
    asset: Option<&str>,
    status: Option<&str>,
    remote: Option<&str>,
    saved: bool,
    back: &str,
) -> Markup {
    html! {form.music-favourite-form method="post" action="/web/music/favourites" {
        input type="hidden" name="csrf" value=(csrf);
        @if let Some(remote)=remote{input type="hidden" name="remote_id" value=(remote);}
        @if let Some(asset)=asset{input type="hidden" name="asset_id" value=(asset);}
        @if let Some(status)=status{input type="hidden" name="status_id" value=(status);}
        input type="hidden" name="operation" value=(if saved{"remove"}else{"save"});
        input type="hidden" name="return_to" value=(back);
        button type="submit"{(l.text(if saved{"music-favourites-remove"}else{"music-favourites-save"}))}
    }}
}
pub async fn change(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    let source = Source {
        remote_id: settings::field(&pairs, "remote_id").map(str::to_owned),
        episode_id: None,
        asset_id: settings::field(&pairs, "asset_id").map(str::to_owned),
        status_id: settings::field(&pairs, "status_id").map(str::to_owned),
    };
    let saved = match settings::field(&pairs, "operation") {
        Some("save") => {
            music_favourites::save(&state, user.current.account.id, &source).await?;
            true
        }
        Some("remove") => {
            music_favourites::remove(&state, user.current.account.id, &source).await?;
            false
        }
        _ => {
            return Err(ApiError::Unprocessable(
                "Invalid favourite operation".into(),
            ));
        }
    };
    if headers
        .get(header::ACCEPT)
        .is_some_and(|v| v == "application/json")
    {
        return Ok((
            [(header::CACHE_CONTROL, "private, no-store")],
            axum::Json(serde_json::json!({"saved":saved})),
        )
            .into_response());
    }
    let back =
        super::actions::safe_return(settings::field(&pairs, "return_to"), "/music/favourites");
    Ok(settings::redirect_to(&back))
}
