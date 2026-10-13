//! Personal collection and unavailable-reference management use native CSRF forms.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_mine::{self, Filter, Request},
    music_saved::{self, Kind, Reference},
};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
fn response(title: &str, user: &WebUser, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(title, Some(user), body),
    )
        .into_response()
}
pub(super) fn form(user: &WebUser, reference: &Reference, saved: bool, back: &str) -> Markup {
    source_form(user.locale, &user.csrf, reference, saved, back)
}
pub(super) fn source_form(
    l: super::i18n::Locale,
    csrf: &str,
    reference: &Reference,
    saved: bool,
    back: &str,
) -> Markup {
    html! {form.music-saved-form method="post" action="/web/music/saved" {
        input type="hidden" name="csrf" value=(csrf);
        input type="hidden" name="kind" value=(reference.kind.text());
        input type="hidden" name="id" value=(reference.id);
        input type="hidden" name="operation" value=(if saved{"remove"}else{"save"});
        input type="hidden" name="return_to" value=(back);
        button type="submit"{(l.text(if saved{"music-mine-remove"}else{"music-mine-save"}))}
    }}
}
pub(super) async fn control(
    state: &AppState,
    user: Option<&WebUser>,
    reference: Reference,
    owned: bool,
    back: &str,
) -> Result<Markup, ApiError> {
    let Some(user) = user else {
        return Ok(html! {});
    };
    let saved = music_saved::contains(state, user.current.account.id, &reference).await?;
    Ok(html! {
        p{a href="/music/mine"{(user.locale.text("music-mine-title"))}
            @if owned{" — " (user.locale.text("music-mine-owned"))}
        }
        @if !owned||saved{(form(user,&reference,saved,back))}
        @if owned{p{a href=(format!("/music/shares/new?kind={}&id={}",reference.kind.text(),reference.id)){(user.locale.text("music-shares-create"))}}}
    })
}
fn membership(user: &WebUser, item: &music_mine::Item) -> Markup {
    let l = user.locale;
    html! {p.music-membership{
        span.music-membership__kind {(super::music_browse::kind_label(l,item.source["resource_kind"].as_str().unwrap_or(item.reference.kind.text())))} " · "
        @if item.membership.owned{span{(l.text("music-mine-owned"))} " "}
        @if item.membership.saved{span{(l.text("music-mine-saved"))} " "}
        @if item.membership.favourite{span{(l.text("music-mine-favourite"))} " "}
        @if item.membership.implicit && !item.membership.owned && !item.membership.saved {span{(l.text("music-mine-related-artist"))}}
    }}
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(input): Query<Request>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let mut page = music_mine::page(&state, user.current.account.id, &input).await?;
    // Listening comes first in the mixed view; the type remains explicit for
    // albums, artists and other entries derived from the same saved music.
    page.items.sort_by_key(|item| {
        let playable = matches!(item.reference.kind, Kind::Track | Kind::Audio)
            || (item.reference.kind == Kind::Remote
                && matches!(
                    item.source["resource_kind"].as_str(),
                    Some("track" | "audio")
                ));
        !playable
    });
    let l = user.locale;
    let title = l.text("music-mine-title");
    let back = format!(
        "/music/mine?{}",
        serde_urlencoded::to_string([("kind", input.kind.text()), ("q", input.q.as_str())])
            .map_err(|error| ApiError::Internal(Box::new(error)))?
    );
    let body = html! {section.column.music-library{
        header.inbox-heading {
            h1{(title)}
            a.button.button--quiet href="/music/assets?mine=true&upload=true"{(l.text("music-upload-audio"))}
        }
        (super::music::workspace_navigation(l,true,"mine"))
        @if !page.items.is_empty() || !input.q.is_empty() || input.kind.text()!="all" {
            form.music-library-filter method="get" action="/music/mine" {
                label.visually-hidden for="music-mine-kind"{(l.text("music-mine-kind"))}
                select id="music-mine-kind" name="kind" {
                    @for (kind,label) in [(Filter::All,"music-mine-all"),(Filter::Tracks,"music-mine-tracks"),(Filter::Artists,"music-mine-artists"),(Filter::Albums,"music-mine-albums"),(Filter::Libraries,"music-mine-libraries"),(Filter::Playlists,"music-mine-playlists"),(Filter::Radios,"music-mine-radios")] {
                        option value=(kind.text()) selected[kind.text()==input.kind.text()]{(l.text(label))}
                    }
                }
                label.visually-hidden for="music-mine-search"{(l.text("music-search"))}
                input id="music-mine-search" type="search" name="q" maxlength="200" value=(input.q) placeholder=(l.text("music-library-search"));
                button.button--quiet type="submit"{(l.text("music-search-submit"))}
            }
        }
        @if page.items.is_empty() {
            section.music-empty-state {
                (super::view::icon("music"))
                h2{(l.text(if input.q.is_empty() && input.kind.text()=="all" {"music-library-start"} else {"music-library-no-results"}))}
                p{(l.text("music-library-empty-hint"))}
                a.button href="/music"{(l.text("music-discovery-browse"))}
            }
        } @else {
            ul.music-listening-results.music-personal-items {@for item in &page.items {li {
                @if let Some(image)=item.source["image"]["url"].as_str().or_else(||item.source["image_url"].as_str()) {
                    img.music-artwork src=(image) width="64" height="64" alt="" loading="lazy";
                } @else {span.music-artwork.music-artwork--placeholder aria-hidden="true"{(super::view::icon("music"))}}
                div.music-track-info {
                    @if let Some(url)=item.source["url"].as_str(){h3{a href=(url){(item.source["name"].as_str().or_else(||item.source["title"].as_str()).unwrap_or_default())}}}
                    (membership(&user,item))
                }
                div.music-row-actions {
                    @if item.reference.kind==Kind::Remote && matches!(item.source["resource_kind"].as_str(),Some("track"|"audio")) {(super::music_player::play_form(&user,"remote",item.reference.key()?.1))}
                    @if matches!(item.reference.kind,Kind::Track|Kind::Audio){(super::music_player::play_form(&user,if item.reference.kind==Kind::Track{"asset"}else{"status"},item.reference.key()?.1))}
                    @if !item.membership.owned||item.membership.saved {
                        details.music-row-menu {summary aria-label=(l.text("inbox-options")){(super::view::icon("more"))}(form(&user,&item.reference,item.membership.saved,&back))}
                    }
                }
            }}}
        }
        details{summary{(l.text("music-library-tools"))}
            a href="/music/saved"{(l.text("music-saved-manager"))}
            a href="/music/shares"{(l.text("music-shares-title"))}
            (super::music_remote::lookup(&user))
        }
        @if let Some(before)=page.before{a href=(format!("{back}&before={before}")){(l.text("music-older"))}}
    }};
    Ok(response(&title, &user, &body))
}
#[derive(Default, serde::Deserialize)]
pub struct ManagerPage {
    max_id: Option<i64>,
}
pub async fn manager(
    State(state): State<AppState>,
    user: WebUser,
    Query(input): Query<ManagerPage>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let page = music_saved::page(&state, user.current.account.id, input.max_id).await?;
    let l = user.locale;
    let title = l.text("music-saved-manager");
    let body = html! {section.column.music-library{
        h1{(title)}p{(l.text("music-saved-manager-hint"))}p{a href="/music/mine"{(l.text("music-mine-title"))}}
        @if page.items.is_empty(){p{(l.text("music-saved-empty"))}}
        ul{@for item in &page.items{li{
            @if let Some(source)=&item.source {
                @if let Some(url)=source["url"].as_str(){a href=(url){(source["name"].as_str().or_else(||source["title"].as_str()).unwrap_or_default())}}
            }@else{span{(l.text("music-saved-unavailable"))}}
            form method="post" action="/web/music/saved/remove"{
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="saved_id" value=(item.id);
                button type="submit"{(l.text("music-mine-remove"))}
            }
        }}}
        @if let Some(before)=page.before{a href=(format!("/music/saved?max_id={before}")){(l.text("music-older"))}}
    }};
    Ok(response(&title, &user, &body))
}
fn pairs(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(pairs)
}
pub async fn change(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = pairs(&user, &body)?;
    let reference = Reference {
        kind: Kind::parse(settings::field(&pairs, "kind").unwrap_or_default())?,
        id: settings::field(&pairs, "id").unwrap_or_default().to_owned(),
    };
    let saved = match settings::field(&pairs, "operation") {
        Some("save") => {
            music_saved::save(&state, user.current.account.id, &reference).await?;
            true
        }
        Some("remove") => {
            music_saved::remove_reference(&state, user.current.account.id, &reference).await?;
            false
        }
        _ => {
            return Err(ApiError::Unprocessable(
                "Invalid music save operation".into(),
            ));
        }
    };
    if headers
        .get(header::ACCEPT)
        .is_some_and(|value| value == "application/json")
    {
        return Ok((
            [(header::CACHE_CONTROL, "private, no-store")],
            axum::Json(serde_json::json!({"saved":saved})),
        )
            .into_response());
    }
    Ok(settings::redirect_to(&super::actions::safe_return(
        settings::field(&pairs, "return_to"),
        "/music/mine",
    )))
}
pub async fn remove(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = pairs(&user, &body)?;
    let id = settings::field(&pairs, "saved_id")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid saved reference".into()))?;
    music_saved::remove(&state, user.current.account.id, id).await?;
    Ok(settings::redirect_to("/music/saved"))
}
