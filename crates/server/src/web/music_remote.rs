//! Remote lookup is an explicit native form within the single Music workspace.
use super::{layout, session::WebUser, settings};
use crate::{AppState, error::ApiError, music_remote};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
pub(super) fn lookup(user: &WebUser) -> maud::Markup {
    let l = user.locale;
    html! {details{summary{(l.text("music-remote-import"))}form method="post" action="/web/music/remote/import"{
        input type="hidden" name="csrf" value=(user.csrf);
        label for="music-remote-url"{(l.text("music-remote-url"))}
        input id="music-remote-url" name="url" type="url" required maxlength="2048";
        button type="submit"{(l.text("music-remote-import"))}
    }}}
}
pub async fn import(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let input = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&input, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    let uri = settings::field(&input, "url").unwrap_or_default();
    let resource = music_remote::import(&state, user.current.account.id, uri).await?;
    Ok(settings::redirect_to(&format!(
        "/music/remote/{}",
        resource.id
    )))
}
#[derive(Default, serde::Deserialize)]
pub struct Page {
    before: Option<i64>,
    offset: Option<usize>,
}
pub async fn show(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let resource = music_remote::get(&state, id, user.current.account.id).await?;
    music_remote::catalogue::current(&state, &resource).await?;
    let catalogue_page = music_remote::catalogue::children(
        &state,
        user.current.account.id,
        &resource,
        page.offset.unwrap_or_default(),
    )
    .await?;
    let catalogue_more = catalogue_page.as_ref().is_some_and(|(_, more)| *more);
    let contents = if let Some((items, _)) = catalogue_page {
        music_remote::Contents {
            items,
            before: None,
        }
    } else {
        music_remote::contents(&state, id, user.current.account.id, page.before).await?
    };
    let playable = match music_remote::stream::source(&state, id, user.current.account.id).await {
        Ok(_) => true,
        Err(ApiError::NotFound) => false,
        Err(error) => return Err(error),
    };
    let favourite = if playable {
        crate::music_favourites::contains(
            &state,
            user.current.account.id,
            &crate::music_queues::Source {
                remote_id: Some(id.to_string()),
                asset_id: None,
                status_id: None,
                episode_id: None,
            },
        )
        .await?
    } else {
        false
    };
    let l = user.locale;
    let source_base = music_remote::catalogue::origin(&resource.uri)?;
    let source_page = source_page(&resource, &source_base);
    let reference = crate::music_saved::Reference {
        kind: crate::music_saved::Kind::Remote,
        id: id.to_string(),
    };
    let back = format!("/music/remote/{id}");
    let saved = crate::music_saved::contains(&state, user.current.account.id, &reference).await?;
    let collection_controls = collection_controls(
        &state,
        &user,
        &resource,
        &contents,
        page.before,
        page.offset,
        playable,
    )
    .await?;
    let body = html! {section.column.music-library{
        h1{(resource.name)}(super::music::workspace_navigation(l,true,"mine"))
        p{a href=(format!("/music/discover?{}",serde_urlencoded::to_string([("server",music_remote::catalogue::origin(&resource.uri)?.as_str())]).unwrap_or_default())){(l.text("music-discovery-server"))}}
        @if let Some(image)=resource.artwork_url() {img.music-artwork src=(image) alt="" loading="lazy" width="256" height="256";}
        @if !resource.summary.is_empty(){details{summary{(l.text("music-about-audio"))}div{(maud::PreEscaped(resource.summary))}}}
        (metadata(l, &resource.details, &resource.kind))
        @if playable {audio controls preload="none" src=(format!("/music/remote/{id}/stream")) aria-label=(l.text("music-player-title")) {}}

        (super::music_saved::form(&user,&reference,saved,&back))
        @if playable {
            (super::music_player::queue_link(&user,"remote",id))
            (super::music_favourites::remote_form(l,&user.csrf,&id.to_string(),favourite,&back))
        }
        @if !playable && contents.items.is_empty(){p{(l.text("music-remote-no-audio"))}}
        (collection_controls)
        @if catalogue_more {a rel="next" href=(format!("{back}?offset={}",page.offset.unwrap_or_default()+music_remote::catalogue::PAGE_SIZE)){(l.text("music-remote-more"))}}
        @if let Some(before)=contents.before {a rel="next" href=(format!("{back}?before={before}")){(l.text("music-older"))}}

        @if let Some(next)=&resource.next_page_uri {form method="post" action="/web/music/remote/import"{
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="url" value=(next);
            button type="submit"{(l.text("music-remote-more"))}
        }}
        details{summary{(l.text("music-resource-tools"))}
            (super::music_activity::link(l,"remote",resource.id))
            p{(l.text("music-remote-origin")) " " a href=(source_page) rel="noopener noreferrer"{(source_base)}}
            @if playable && resource.catalogue_kind.as_deref()!=Some("stream") {(super::music_downloads::link(&user,"remote",id))}
        form method="post" action="/web/music/remote/import"{
            input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="url" value=(resource.uri);
            button type="submit"{(l.text("music-remote-refresh"))}
        }
        }
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        layout::shell(&resource.name, Some(&user), &body),
    )
        .into_response())
}

fn metadata(
    l: super::i18n::Locale,
    detail: &music_remote::details::Details,
    kind: &str,
) -> maud::Markup {
    html! {
        dl.music-metadata {
            @for (label,value) in [("music-artist",&detail.artist),("music-album",&detail.album),("music-genre",&detail.genre)] {
                @if !value.is_empty() {dt{(l.text(label))}dd{
                    @if label=="music-artist" && let Some(uri)=&detail.artist_uri {a href=(format!("/music/open?{}",serde_urlencoded::to_string([("url",uri)]).unwrap_or_default())){(value)}} @else {(value)}
                }}
            }
            @for (label,value) in [("music-year",detail.release_year),("music-track-number",detail.track_number),("music-disc-number",detail.disc_number)] {
                @if let Some(value)=value {dt{(l.text(label))}dd{(value)}}
            }
            @if let Some(mbid)=&detail.musicbrainz_id {dt{(l.text(if matches!(kind,"artist"|"album"){ "music-entity-mbid"}else{"music-mbid"}))}dd{(mbid)}}
            @if !detail.tags.is_empty() {dt{(l.text("music-entity-tags"))}dd{(detail.tags.join(", "))}}
        }
        @if let Some(seconds)=detail.duration {(super::music::total_duration(l,seconds))}
    }
}

async fn collection_controls(
    state: &AppState,
    user: &WebUser,
    root: &music_remote::Resource,
    contents: &music_remote::Contents,
    before: Option<i64>,
    offset: Option<usize>,
    playable: bool,
) -> Result<maud::Markup, ApiError> {
    let collections: Vec<crate::music_collections::Collection> = sqlx::query_as(
        "SELECT * FROM music_collections WHERE owner_id=$1 ORDER BY lower(name),id LIMIT 100",
    )
    .bind(user.current.account.id)
    .fetch_all(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let ids = contents
        .items
        .iter()
        .map(|item| item.id)
        .collect::<Vec<_>>();
    let tracks = music_remote::playables_pool(
        &state.pool,
        &state.config.domain,
        &ids,
        Some(user.current.account.id),
    )
    .await?;
    let l = user.locale;
    Ok(html! {
        @if !tracks.is_empty() {form method="post" action=(format!("/web/music/remote/{}/play",root.id)){
            input type="hidden" name="csrf" value=(user.csrf);@if let Some(offset)=offset{input type="hidden" name="offset" value=(offset);}@if let Some(before)=before{input type="hidden" name="before" value=(before);}
            button type="submit"{(l.text("music-play-all"))}
        }}
        @if !contents.items.is_empty(){h2{(l.text(if contents.items.iter().all(|child|matches!(child.kind.as_str(),"track"|"audio")){"music-remote-contents"}else{"music-title"}))}ul{@for child in &contents.items {li{
            a href=(format!("/music/remote/{}",child.id)){(child.name)}
            @if tracks.contains_key(&child.id) {
                audio controls preload="none" src=(format!("/music/remote/{}/stream",child.id)) aria-label=(child.name){}
            }
        }}}}
        @if !collections.is_empty() && (playable || !tracks.is_empty()) {details{summary{(l.text("music-organise-playlist"))}
        @if !collections.is_empty() && (playable || !tracks.is_empty()) {
            form.settings-form id="remote-collect" method="post" action=(format!("/web/music/remote/{}/collect",root.id)) {
                input type="hidden" name="csrf" value=(user.csrf);
                @if let Some(offset)=offset {input type="hidden" name="offset" value=(offset);}
                @if let Some(before)=before {input type="hidden" name="before" value=(before);}
                @if playable {input type="hidden" name="remote" value=(root.id);}
                label for="remote-collection"{(l.text("music-collections-title"))}
                select id="remote-collection" name="collection"{@for collection in &collections {option value=(format!("{}:{}",collection.id,collection.revision)){(collection.name)}}}
                @for child in &contents.items {@if tracks.contains_key(&child.id) {label{input type="checkbox" name="remote" value=(child.id) checked;(child.name)}}}
                button type="submit"{(l.text("music-collection-add"))}
            }
        }
        }}
    })
}

pub async fn collect(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:statuses")?;
    let fields = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    let (collection, revision) = settings::field(&fields, "collection")
        .and_then(|value| value.split_once(':'))
        .and_then(|(collection, revision)| {
            Some((
                collection.parse::<i64>().ok()?,
                revision.parse::<i64>().ok()?,
            ))
        })
        .ok_or_else(|| ApiError::Unprocessable("Choose a music collection".into()))?;
    let before = settings::field(&fields, "before")
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| ApiError::NotFound)?;
    let root = music_remote::get(&state, id, user.current.account.id).await?;
    let offset = settings::field(&fields, "offset")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| ApiError::NotFound)?
        .unwrap_or_default();
    let contents = if let Some((items, _)) =
        music_remote::catalogue::children(&state, user.current.account.id, &root, offset).await?
    {
        music_remote::Contents {
            items,
            before: None,
        }
    } else {
        music_remote::contents(&state, id, user.current.account.id, before).await?
    };
    let selected = fields
        .iter()
        .filter(|(key, _)| key == "remote")
        .map(|(_, value)| value.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    if selected.iter().any(|selected| {
        *selected != root.id && !contents.items.iter().any(|item| item.id == *selected)
    }) {
        return Err(ApiError::NotFound);
    }
    crate::music_collections::add_remote_sources(
        &state,
        user.current.account.id,
        collection,
        revision,
        &selected,
    )
    .await?;
    Ok(settings::redirect_to(&format!(
        "/music/collections/{collection}"
    )))
}

/// Play the visible album page in one atomic queue change, without saving/following.
pub async fn play(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let before = settings::field(&fields, "before")
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| ApiError::NotFound)?;
    let root = music_remote::get(&state, id, user.current.account.id).await?;
    let offset = settings::field(&fields, "offset")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| ApiError::NotFound)?
        .unwrap_or_default();
    let contents = if let Some((items, _)) =
        music_remote::catalogue::children(&state, user.current.account.id, &root, offset).await?
    {
        music_remote::Contents {
            items,
            before: None,
        }
    } else {
        music_remote::contents(&state, id, user.current.account.id, before).await?
    };
    let ids = contents.items.iter().map(|r| r.id).collect::<Vec<_>>();
    let playable = music_remote::playables_pool(
        &state.pool,
        &state.config.domain,
        &ids,
        Some(user.current.account.id),
    )
    .await?;
    let sources = ids
        .iter()
        .filter(|id| playable.contains_key(id))
        .map(|id| crate::music_queues::Source {
            remote_id: Some(id.to_string()),
            asset_id: None,
            status_id: None,
            episode_id: None,
        })
        .collect::<Vec<_>>();
    if sources.is_empty() {
        return Err(ApiError::NotFound);
    }
    let queue = crate::music_queues::get(&state, user.current.account.id, "web").await?;
    crate::music_queues::change(
        &state,
        user.current.account.id,
        "web",
        &crate::music_queues::Change {
            revision: queue.revision,
            action: crate::music_queues::Action::Play { sources },
        },
    )
    .await?;
    Ok(settings::redirect_to("/music/player?autoplay=true"))
}

fn source_page(resource: &music_remote::Resource, base: &str) -> String {
    match resource.catalogue_kind.as_deref() {
        Some("track") => format!(
            "{base}/tracks/{}",
            resource.catalogue_key.as_deref().unwrap_or_default()
        ),
        Some("stream") => format!("{base}/streams"),
        _ => resource.uri.replace("/api/v1/radios/", "/radios/"),
    }
}
