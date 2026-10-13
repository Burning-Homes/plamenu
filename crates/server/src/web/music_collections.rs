//! Named libraries and playlists in the existing server-rendered music design.
use super::{
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_collections::{self, Add, Collection, New, Reorder},
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct Page {
    pub q: Option<String>,
    pub kind: Option<String>,
    pub mine: Option<bool>,
    pub max_id: Option<i64>,
    pub after: Option<i64>,
}
fn response(title: &str, user: Option<&WebUser>, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(title, user, body),
    )
        .into_response()
}
fn fields(l: super::i18n::Locale, item: Option<&Collection>) -> Markup {
    html! {
        label for="music-collection-name"{(l.text("music-collection-name"))}
        input id="music-collection-name" name="name" required maxlength="200" value=(item.map_or("",|v|v.name.as_str()));
        label for="music-collection-description"{(l.text("music-collection-description"))}
        textarea id="music-collection-description" name="description" maxlength="4000"{(item.map_or("",|v|v.description.as_str()))}
        @if let Some(item)=item {
            input type="hidden" name="kind" value=(item.kind);
            input type="hidden" name="revision" value=(item.revision);
        } @else {
            label for="music-collection-kind"{(l.text("music-collection-kind"))}
            select id="music-collection-kind" name="kind"{option value="library"{(l.text("music-collection-library"))}option value="playlist"{(l.text("music-collection-playlist"))}}
        }
        label for="music-collection-visibility"{(l.text("music-assets-visibility"))}
        select id="music-collection-visibility" name="visibility"{
            @for (value,label) in [("private","music-assets-private"),("local","music-assets-local"),("public","music-public")]{option value=(value) selected[item.map_or(value=="private",|v|v.visibility==value)]{(l.text(label))}}
        }
    }
}
pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(q): Query<Page>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    if let Some(response) = super::session::preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_federated,
    ) {
        return Ok(response);
    }
    let mut items = music_collections::search(
        &state,
        user.as_ref().map(|u| u.current.account.id),
        q.mine.unwrap_or(false),
        q.max_id,
        q.q.as_deref().unwrap_or_default(),
        q.kind.as_deref().filter(|v| !v.is_empty()),
    )
    .await?;
    let before = if items.len() > 60 {
        items.truncate(60);
        items.last().map(|v| v.id)
    } else {
        None
    };
    let body = html! {section.column.music-library{
        h1{(l.text("music-collections-title"))}
        (super::music::workspace_navigation(l,user.is_some(),"collections"))
        nav.video-explore__links aria-label=(l.text("music-collections-title")){
            a href="/music"{(l.text("music-title"))} a href="/music/assets"{(l.text("music-assets-title"))}
            a href="/music/collections"{(l.text("music-public"))}
            @if user.is_some(){a href="/music/collections?mine=true"{(l.text("music-collections-mine"))}}
        }
        form.settings-form method="get" action="/music/collections"{
            @if q.mine.unwrap_or(false){input type="hidden" name="mine" value="true";}
            label for="music-collection-search"{(l.text("music-collections-search"))}input id="music-collection-search" type="search" name="q" maxlength="200" value=(q.q.as_deref().unwrap_or_default());
            label for="music-collection-filter"{(l.text("music-collection-kind"))}select id="music-collection-filter" name="kind"{@for(value,key)in[("","music-collections-all"),("library","music-collection-library"),("playlist","music-collection-playlist")]{option value=(value) selected[q.kind.as_deref().unwrap_or_default()==value]{(l.text(key))}}}
            button type="submit"{(l.text("music-search-submit"))}
        }
        @if let Some(user)=&user {details{summary{(l.text("music-collection-create"))}
            form.settings-form method="post" action="/web/music/collections/create"{
                input type="hidden" name="csrf" value=(user.csrf);(fields(l,None))
                button type="submit"{(l.text("music-collection-create"))}
            }
        }}
        @if items.is_empty(){p{(l.text("music-collections-empty"))}}
        ul{@for item in &items{li{a href=(format!("/music/collections/{}",item.id)){(item.name)} " — " (l.text(if item.kind=="library"{"music-collection-library"}else{"music-collection-playlist"}))}}}
        @if let Some(before)=before{a href=(format!("/music/collections?mine={}&max_id={before}&{}",q.mine.unwrap_or(false),serde_urlencoded::to_string([("q",q.q.as_deref().unwrap_or_default()),("kind",q.kind.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(
        &l.text("music-collections-title"),
        user.as_ref(),
        &body,
    ))
}
fn sharing_form(user: &WebUser, item: &Collection) -> Markup {
    let l = user.locale;
    let id = item.id;
    html! {
            form.settings-form method="post" action=(format!("/web/music/collections/{id}/contents-sharing")){
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="revision" value=(item.revision);
                label for="music-collection-share-tracks"{
                    input id="music-collection-share-tracks" name="share_tracks" type="checkbox" value="true" checked[item.share_tracks] aria-describedby="music-collection-sharing-hint";
                    (l.text("music-collection-share-tracks"))
                }
                p id="music-collection-sharing-hint"{(l.text("music-collection-sharing-hint"))}
                button type="submit"{(l.text("music-collection-sharing-save"))}
            }
    }
}
fn grant_form(user: &WebUser, item: &Collection) -> Markup {
    let l = user.locale;
    let id = item.id;
    html! {
            details{summary{(l.text("music-assets-share"))}
                form.settings-form method="post" action=(format!("/web/music/collections/{id}/grant")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="revision" value=(item.revision);
                    label for="music-collection-member"{(l.text("music-assets-member"))}
                    input id="music-collection-member" name="member" required maxlength="200";
                    button type="submit" name="operation" value="grant"{(l.text("music-assets-grant"))}
                    button type="submit" name="operation" value="revoke"{(l.text("music-assets-revoke"))}
                }
            }
    }
}
async fn saved_control(
    state: &AppState,
    user: Option<&WebUser>,
    item: &Collection,
) -> Result<Markup, ApiError> {
    let id = item.id;
    let own = user.is_some_and(|user| user.current.account.id == item.owner_id);
    super::music_saved::control(
        state,
        user,
        crate::music_saved::Reference {
            kind: if item.kind == "library" {
                crate::music_saved::Kind::Library
            } else {
                crate::music_saved::Kind::Playlist
            },
            id: id.to_string(),
        },
        own,
        &format!("/music/collections/{id}"),
    )
    .await
}
#[allow(
    clippy::too_many_lines,
    reason = "native resource page assembles its authorized controls and website cards"
)]
pub async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let item = music_collections::get(&state, id, viewer).await?;
    let artworks = crate::music_artworks::for_target(
        &state,
        crate::music_artworks::Target::Collections,
        id,
        viewer,
    )
    .await?;
    let own = viewer == Some(item.owner_id);
    let collected = saved_control(&state, user.as_ref(), &item).await?;
    let counts = music_collections::statistics(&state, id, viewer).await?;
    let (entries, tracks, after) = if own {
        let page = music_collections::entry_page(&state, id, q.after).await?;
        let tracks = music_collections::page_sources(&state, id, viewer, &page).await?;
        (page.items, tracks, page.after)
    } else {
        let (tracks, after) = music_collections::playable_page(&state, id, viewer, q.after).await?;
        (Vec::new(), tracks, after)
    };
    let track_map = tracks
        .iter()
        .map(|t| (t.entry_id.as_str(), t))
        .collect::<std::collections::HashMap<_, _>>();
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    let body = html! {section.column.music-library{
        h1{(item.name)}
        (super::music::workspace_navigation(l,user.is_some(),"collections"))
        p{a href=(format!("/music/feeds/collections/{id}.rss")){(l.text("music-feed-rss"))} " · " a href=(format!("/music/feeds/collections/{id}.atom")){(l.text("music-feed-atom"))}} (super::music_artworks::images(l,&item.name,&artworks)) @if !item.description.is_empty(){p{(item.description)}}
        (collected)
        @if !item.tags.is_empty(){p{(l.text("music-entity-tags")) ": " (super::music::term_links(l,crate::music_feeds::Term::Tag,&item.tags))}}
        p{(l.text("music-entity-tracks")) ": " (counts.track_count)}
        (super::music::total_duration(l,counts.total_duration))
        @if let Some(user)=&user{(super::music_player::link(user,"collection",id))(super::music_downloads::link(user,"collection",id))}
        @if item.kind=="library" && item.visibility=="public" {
            p{a href=(crate::music_library_federation::uri(&state.config.domain,id)){(l.text("music-collection-federation"))}}
        }
        @if item.visibility=="public"{(super::music_embeds::link(l,"collections",id))}
        p{a href=(format!("/music/collections/{id}/export.m3u")){(l.text("music-collection-export"))}}
        @if !own {
            ol{@for track in &tracks{li{
                a href=(track.url){(track.title)} " — " (track.artist)
                audio controls preload="none" aria-label=(track.title) src=(track.stream_url){}
            }}}
        }
        @if let Some(user)=&user {@if own {
            (super::music_artworks::forms(user,crate::music_artworks::Target::Collections,id,item.revision,&artworks))
            p{(l.text("music-collection-access-hint"))}
            details{summary{(l.text("music-collection-sharing-settings"))}(sharing_form(user,&item))}
            details{summary{(l.text("music-collection-tags"))}form.settings-form method="post" action=(format!("/web/music/collections/{id}/tags")){
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(item.revision);
                label for="music-collection-tags"{(l.text("music-entity-tags"))}input id="music-collection-tags" name="tags" value=(item.tags.join(", ")) maxlength="5050";
                button type="submit"{(l.text("music-collection-save-tags"))}
            }}
            details{summary{(l.text("music-collection-edit"))}
                form.settings-form method="post" action=(format!("/web/music/collections/{id}/edit")){
                    input type="hidden" name="csrf" value=(user.csrf);(fields(l,Some(&item)))
                    button type="submit"{(l.text("music-assets-save"))}
                }
            }
            details{summary{(l.text("music-collection-add"))}
                form.settings-form method="post" action=(format!("/web/music/collections/{id}/add")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="revision" value=(item.revision);
                    label for="music-collection-track-url"{(l.text("music-collection-track-url"))}
                    input id="music-collection-track-url" name="url" type="url" maxlength="2048" required;
                    p{(l.text("music-collection-url-hint"))}
                    button type="submit"{(l.text("music-collection-add"))}
                }
            }
            ol{@for (position,entry) in entries.iter().enumerate(){li{
                @if let Some(track)=track_map.get(entry.id.to_string().as_str()) {
                    a href=(track.url){(track.title)} " — " (track.artist)
                    audio controls preload="none" aria-label=(track.title) src=(track.stream_url){}
                }@else{span{(l.text("music-collection-unavailable"))}}
                form method="post" action=(format!("/web/music/collections/{id}/entry")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="revision" value=(item.revision);
                    input type="hidden" name="entry" value=(entry.id);
                    button type="submit" name="operation" value="up" disabled[entry.position==1]{(l.text("music-collection-up"))}
                    button type="submit" name="operation" value="down" disabled[after.is_none() && position+1==entries.len()]{(l.text("music-collection-down"))}
                    button type="submit" name="operation" value="remove"{(l.text("music-collection-remove"))}
                }
            }}}
            details{summary{(l.text("music-collection-member-access"))}(grant_form(user,&item))}
            form method="post" action=(format!("/web/music/collections/{id}/delete")){
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="revision" value=(item.revision);
                button.settings-button--danger type="submit"{(l.text("music-collection-delete"))}
            }
        }}
        details{summary{(l.text("music-resource-tools"))}
        (super::music_activity::link(l,"collections",item.id))
        @if item.kind=="library" {(super::music::subscription(&state,user.as_ref(),&crate::music_library_federation::uri(&state.config.domain,id),&format!("/music/collections/{id}")).await?)}
        (super::webmentions::for_target(
        &state,
        &format!("https://{}/music/collections/{id}", state.config.domain),
        l,
    )
    .await?)
        }
        @if let Some(after)=after{p{a href=(format!("/music/collections/{id}?after={after}")){(l.text("music-older"))}}}
        p{a href="/music/collections"{(l.text("music-collections-title"))}}
    }};
    Ok(feed_response(&item.name, user.as_ref(), &body, id, l))
}
fn feed_response(
    title: &str,
    user: Option<&WebUser>,
    body: &Markup,
    id: i64,
    locale: super::i18n::Locale,
) -> Response {
    let meta = layout::PageMeta {
        feed_url: Some(format!("/music/feeds/collections/{id}.atom")),
        rss_feed_url: Some(format!("/music/feeds/collections/{id}.rss")),
        ..Default::default()
    };
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell_with_metadata(title, user, body, &meta, locale),
    )
        .into_response()
}

fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let input = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&input, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(input)
}
fn attributes(input: &[(String, String)]) -> New {
    let field = |key| settings::field(input, key).unwrap_or_default().to_owned();
    New {
        name: field("name"),
        description: field("description"),
        kind: field("kind"),
        visibility: field("visibility"),
    }
}
fn revision(input: &[(String, String)]) -> Result<i64, ApiError> {
    settings::field(input, "revision")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::Unprocessable("Invalid collection revision".into()))
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    let item =
        music_collections::create(&state, user.current.account.id, &attributes(&input)).await?;
    Ok(settings::redirect_to(&format!(
        "/music/collections/{}",
        item.id
    )))
}
pub async fn edit(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    music_collections::update(
        &state,
        user.current.account.id,
        id,
        revision(&input)?,
        &attributes(&input),
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}
pub async fn contents_sharing(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    let enabled = match settings::field(&input, "share_tracks") {
        Some("true") => true,
        None => false,
        _ => {
            return Err(ApiError::Unprocessable(
                "Invalid contents sharing choice".into(),
            ));
        }
    };
    music_collections::set_contents_sharing(
        &state,
        user.current.account.id,
        id,
        revision(&input)?,
        enabled,
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    music_collections::delete(&state, user.current.account.id, id, revision(&input)?).await?;
    Ok(settings::redirect_to("/music/collections?mine=true"))
}
pub async fn add(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    let url = settings::field(&input, "url").unwrap_or_default();
    let remote_id = resolve_remote_reference(&state, user.current.account.id, url).await?;
    let episode_id = if remote_id.is_none() {
        music_collections::resolve_episode_reference(&state, url).await?
    } else {
        None
    };
    let (asset_id, status_id) = if episode_id.is_some() || remote_id.is_some() {
        (None, None)
    } else {
        music_collections::resolve_reference(&state, url).await?
    };
    music_collections::add(
        &state,
        user.current.account.id,
        id,
        &Add {
            remote_id,
            episode_id,
            asset_id,
            status_id,
            revision: revision(&input)?.to_string(),
        },
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}
pub async fn entry(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    let revision = revision(&input)?;
    let entry = settings::field(&input, "entry")
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| ApiError::Unprocessable("Invalid music entry".into()))?;
    match settings::field(&input, "operation") {
        Some("remove") => {
            music_collections::remove(&state, user.current.account.id, id, revision, entry).await?;
        }
        Some(operation @ ("up" | "down")) => {
            let current = music_collections::get(&state, id, Some(user.current.account.id)).await?;
            if current.owner_id != user.current.account.id {
                return Err(ApiError::NotFound);
            }
            let mut ids = music_collections::entries(&state, id)
                .await?
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>();
            let position = ids
                .iter()
                .position(|e| *e == entry)
                .ok_or(ApiError::NotFound)?;
            let target = if operation == "up" {
                position.checked_sub(1)
            } else {
                position.checked_add(1).filter(|p| *p < ids.len())
            };
            let target = target
                .ok_or_else(|| ApiError::Unprocessable("Entry is already at the end".into()))?;
            ids.swap(position, target);
            music_collections::reorder(
                &state,
                user.current.account.id,
                id,
                &Reorder {
                    revision: revision.to_string(),
                    entry_ids: ids.into_iter().map(|id| id.to_string()).collect(),
                },
            )
            .await?;
        }
        _ => return Err(ApiError::Unprocessable("Choose move or remove".into())),
    }
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}

pub async fn grant(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let input = form(&user, &body)?;
    let target = plamenu_db::account::find_local_by_username(
        &state.pool,
        settings::field(&input, "member").unwrap_or_default(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let enabled = match settings::field(&input, "operation") {
        Some("grant") => true,
        Some("revoke") => false,
        _ => return Err(ApiError::Unprocessable("Choose grant or revoke".into())),
    };
    music_collections::grant(
        &state,
        user.current.account.id,
        id,
        revision(&input)?,
        target.id,
        enabled,
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}

pub async fn tags(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let input = form(&user, &body)?;
    let tags = settings::field(&input, "tags")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect();
    let input = music_collections::Tags {
        revision: settings::field(&input, "revision")
            .unwrap_or_default()
            .to_owned(),
        tags,
    };
    music_collections::tags(
        &state,
        user.current.account.id,
        id,
        &input,
        music_collections::TagAction::Replace,
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/collections/{id}")))
}

async fn resolve_remote_reference(
    state: &AppState,
    viewer: i64,
    value: &str,
) -> Result<Option<String>, ApiError> {
    let path = url::Url::parse(value)
        .ok()
        .and_then(|url| {
            (url.host_str() == Some(state.config.domain.as_str())).then(|| url.path().to_owned())
        })
        .unwrap_or_else(|| value.to_owned());
    if let Some(id) = path
        .strip_prefix("/music/remote/")
        .and_then(|id| id.parse::<i64>().ok())
    {
        return Ok(Some(
            crate::music_remote::get(state, id, viewer)
                .await?
                .id
                .to_string(),
        ));
    }
    Ok(None)
}
