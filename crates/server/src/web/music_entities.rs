//! Artist/album pages share the stored-track authorization and native layout.
use super::{
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_entities::{self, Kind},
};
use axum::body::Bytes;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct Page {
    pub q: Option<String>,
    pub mine: Option<bool>,
    pub max_id: Option<i64>,
    pub remote_before: Option<i64>,
    pub after: Option<i64>,
    pub artist_id: Option<i64>,
}
fn title_key(kind: Kind) -> &'static str {
    match kind {
        Kind::Artist => "music-artists-title",
        Kind::Album => "music-albums-title",
    }
}
fn response(title: &str, user: Option<&WebUser>, body: &maud::Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(title, user, body),
    )
        .into_response()
}
async fn index(
    state: AppState,
    kind: Kind,
    user: Option<WebUser>,
    headers: HeaderMap,
    q: Page,
) -> Result<Response, ApiError> {
    if let Some(response) = super::session::preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_local,
    ) {
        return Ok(response);
    }
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let mine = q.mine.unwrap_or(false);
    let search = q.q.as_deref().unwrap_or_default();
    let mut entries =
        music_entities::catalogue(&state, kind, viewer, mine, search, q.max_id).await?;
    let before = if entries.len() > 60 {
        entries.truncate(60);
        entries.last().map(|e| e.id)
    } else {
        None
    };
    let remote = if mine {
        html! {}
    } else {
        super::music_browse::remote_section(
            &state,
            user.as_ref(),
            l,
            search,
            q.remote_before,
            if kind == Kind::Album {
                "album"
            } else {
                "artist"
            },
        )
        .await?
    };
    let body = html! {section.column.music-library{
        h1{(l.text(title_key(kind)))}
        (super::music::workspace_navigation(l,user.is_some(),kind.path()))
        nav.video-explore__links aria-label=(l.text("music-title")){
            a href="/music"{(l.text("music-title"))} a href="/music/assets"{(l.text("music-assets-title"))}
            a href="/music/artists"{(l.text("music-artists-title"))} a href="/music/albums"{(l.text("music-albums-title"))}
            @if user.is_some(){a href=(format!("/music/{}?mine=true",kind.path())){(l.text("music-entities-mine"))}}
        }
        form.settings-form method="get" action=(format!("/music/{}",kind.path())){
            @if mine{input type="hidden" name="mine" value="true";}
            label for="music-entity-search"{(l.text("music-search"))}
            input id="music-entity-search" type="search" name="q" maxlength="200" value=(search);
            button type="submit"{(l.text("music-search-submit"))}
        }
        @if user.is_some(){p{a href=(format!("/music/{}/new",kind.path())){(l.text(create_key(kind)))}}}
        @if entries.is_empty() && remote.0.is_empty(){p{(l.text("music-browse-empty"))}}
        ul{@for entry in &entries{li{a href=(format!("/music/{}/{}",kind.path(),entry.id)){(entry.name)}
            @if kind==Kind::Album{" — "(entry.artist) @if let Some(year)=entry.release_year{" · "(year)}}
        }}}
        (remote)
        @if let Some(before)=before{a href=(format!("/music/{}?mine={mine}&max_id={before}&{}",kind.path(),serde_urlencoded::to_string([("q",search)]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(&l.text(title_key(kind)), user.as_ref(), &body))
}
#[allow(
    clippy::too_many_lines,
    reason = "native resource page assembles its authorized controls and website cards"
)]
async fn show(
    state: AppState,
    kind: Kind,
    id: i64,
    user: Option<WebUser>,
    headers: HeaderMap,
    q: Page,
) -> Result<Response, ApiError> {
    if let Some(response) = super::session::preview_redirect(
        &user,
        state
            .settings_cache
            .get(&state.pool)
            .await?
            .timeline_preview_local,
    ) {
        return Ok(response);
    }
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    let viewer = user.as_ref().map(|u| u.current.account.id);
    let entity = music_entities::get(&state, kind, id, viewer).await?;
    let collected = saved_control(&state, user.as_ref(), kind, id, entity.owner_id).await?;
    let target = if kind == Kind::Artist {
        crate::music_artworks::Target::Artists
    } else {
        crate::music_artworks::Target::Albums
    };
    let artworks = crate::music_artworks::for_target(&state, target, id, viewer).await?;
    let counts = music_entities::statistics(&state, kind, id, viewer).await?;
    let page = music_entities::tracks(&state, kind, id, viewer, q.after).await?;
    let embeddable = super::music_embeds::available(&state, kind.path(), id).await?;
    let body = html! {section.column.music-library{
        h1{(entity.name)}
        (super::music::workspace_navigation(l,user.is_some(),kind.path()))
        (collected)
        (super::music_artworks::images(l,&entity.name,&artworks))
        @if kind==Kind::Album{p{a href=(format!("/music/artists/{}",entity.artist_id.unwrap_or_default())){(entity.artist)} @if let Some(year)=entity.release_year{" · "(year)}}}
        @if !entity.description.is_empty(){p{(entity.description)}}
        @if !entity.tags.is_empty(){p{(l.text("music-entity-tags")) ": "(super::music::term_links(l,crate::music_feeds::Term::Tag,&entity.tags))}}
        @if !entity.genres.is_empty(){p{(l.text("music-entity-genres")) ": "(super::music::term_links(l,crate::music_feeds::Term::Genre,&entity.genres))}}
        p{(l.text("music-entity-tracks")) ": "(counts.track_count)}
        @if kind==Kind::Artist{p{(l.text("music-entity-albums")) ": "(counts.album_count)}}
        (super::music::total_duration(l,counts.total_duration))
        @if !page.items.is_empty() && let Some(user)=&user{(super::music_player::group_play_form(user,if kind==Kind::Album{"album"}else{"artist"},id,q.after))}
        @if let Some(user)=&user {(super::music_downloads::link(user,if kind==Kind::Artist{"artist"}else{"album"},id))}
        ol.music-album-tracks{@for track in &page.items{li{
            div {a href=(format!("/music/assets/{}",track.id)){(track.title)}
            @if let Some(disc)=track.disc_number{" · "(l.text("music-disc-number")) ": "(disc)}
            @if let Some(number)=track.track_number{" · "(l.text("music-track-number")) ": "(number)}
            }
            @if let Some(user)=&user {(super::music_player::play_form(user,"asset",track.id))}
            @else {audio controls preload="none" aria-label=(track.title) src=(format!("/music/assets/{}/stream",track.id)){}}
        }}}
        @if viewer==Some(entity.owner_id) {details.music-management {summary {(l.text("music-manage-catalogue"))}
        @if let Some(owner)=&user { @if viewer==Some(entity.owner_id){
            p{(l.text(if kind==Kind::Album{"music-album-access-hint"}else{"music-entity-access-hint"}))}
            @if kind==Kind::Album{p{a href=(format!("/music/albums/{id}/access")){(l.text("music-album-access-title"))}}}
            @if kind==Kind::Artist{p{a href=(format!("/music/albums/new?artist_id={id}")){(l.text("music-album-create"))}}}
            (super::music_artworks::forms(owner,target,id,entity.revision,&artworks))
            details {summary{(l.text(if kind==Kind::Artist {"music-artist-edit"} else {"music-album-edit"}))}
            form.settings-form method="post" action=(format!("/web/music/{}/{id}/edit",kind.path())){
                input type="hidden" name="csrf" value=(owner.csrf);
                input type="hidden" name="revision" value=(entity.revision);
                label for="entity-name"{(l.text("music-entity-name"))}
                input id="entity-name" name="name" required maxlength="200" value=(entity.name);
                label for="entity-description"{(l.text("music-entity-description"))}
                textarea id="entity-description" name="description" maxlength="4000"{(entity.description)}
                label for="entity-mbid"{(l.text("music-entity-mbid"))}
                input id="entity-mbid" name="musicbrainz_id" value=(entity.musicbrainz_id.as_deref().unwrap_or_default()) maxlength="36";
                label for="entity-tags"{(l.text("music-entity-tags"))}
                input id="entity-tags" name="tags" value=(entity.tags.join(", "));
                p{(l.text("music-entity-terms-hint"))}
                @if kind==Kind::Album{
                    label for="entity-genres"{(l.text("music-entity-genres"))}
                    input id="entity-genres" name="genres" value=(entity.genres.join(", "));
                    label for="entity-year"{(l.text("music-year"))}
                    input id="entity-year" name="release_year" type="number" min="1" max="9999" value=(entity.release_year.map(|v|v.to_string()).unwrap_or_default());
                }
                button type="submit"{(l.text("music-entity-save"))}
            }}
            details{summary{(l.text("music-entity-delete"))}
                p{(l.text("music-entity-delete-hint"))}
                form.settings-form method="post" action=(format!("/web/music/{}/{id}/delete",kind.path())){
                    input type="hidden" name="csrf" value=(owner.csrf);
                    input type="hidden" name="revision" value=(entity.revision);
                    label {input type="checkbox" name="recursive" value="true";" "(l.text("music-entity-delete-recursive"))}
                    button.settings-button--danger type="submit"{(l.text("music-entity-delete"))}
                }
            }
        }}
        }}
        details{summary{(l.text("music-resource-tools"))}
        (super::music_activity::link(l,kind.path(),id))
        (super::music::subscription(&state,user.as_ref(),&music_entities::uri(&state.config.domain,kind,id),&format!("/music/{}/{id}",kind.path())).await?)
        (super::webmentions::for_target(
        &state,
        &format!("https://{}/music/{}/{id}", state.config.domain, kind.path()),
        l,
    )
    .await?)
        @if embeddable{(super::music_embeds::link(l,kind.path(),id))}
        @if kind==Kind::Artist{p{a href=(format!("/music/feeds/artists/{id}.rss")){(l.text("music-feed-rss"))} " · " a href=(format!("/music/feeds/artists/{id}.atom")){(l.text("music-feed-atom"))}}}
        @if let Some(mbid)=&entity.musicbrainz_id{p{(l.text("music-entity-mbid")) ": "(mbid)}}
        }
        @if let Some(after)=page.after{a href=(format!("/music/{}/{id}?after={after}",kind.path())){(l.text("music-older"))}}
        a href=(format!("/music/{}",kind.path())){(l.text(title_key(kind)))}
    }};
    Ok(entity_response(
        &entity.name,
        user.as_ref(),
        &body,
        (kind, id),
        l,
    ))
}
async fn saved_control(
    state: &AppState,
    user: Option<&WebUser>,
    kind: Kind,
    id: i64,
    owner: i64,
) -> Result<maud::Markup, ApiError> {
    super::music_saved::control(
        state,
        user,
        crate::music_saved::Reference {
            kind: if kind == Kind::Artist {
                crate::music_saved::Kind::Artist
            } else {
                crate::music_saved::Kind::Album
            },
            id: id.to_string(),
        },
        user.is_some_and(|user| user.current.account.id == owner),
        &format!("/music/{}/{id}", kind.path()),
    )
    .await
}
fn entity_response(
    title: &str,
    user: Option<&WebUser>,
    body: &maud::Markup,
    resource: (Kind, i64),
    locale: super::i18n::Locale,
) -> Response {
    let (kind, id) = resource;
    let meta = if kind == Kind::Artist {
        layout::PageMeta {
            feed_url: Some(format!("/music/feeds/artists/{id}.atom")),
            rss_feed_url: Some(format!("/music/feeds/artists/{id}.rss")),
            ..Default::default()
        }
    } else {
        layout::PageMeta::default()
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
pub async fn artists(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    index(state, Kind::Artist, user, headers, q).await
}
pub async fn albums(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    index(state, Kind::Album, user, headers, q).await
}
pub async fn artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    show(state, Kind::Artist, id, user, headers, q).await
}
pub async fn album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    show(state, Kind::Album, id, user, headers, q).await
}

async fn edit(
    state: AppState,
    user: WebUser,
    kind: Kind,
    id: i64,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let field = |key| settings::field(&form, key).unwrap_or_default().to_owned();
    let terms = |key| {
        field(key)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let year = field("release_year");
    let input = music_entities::Edit {
        name: field("name"),
        description: field("description"),
        revision: field("revision"),
        musicbrainz_id: Some(field("musicbrainz_id")).filter(|v| !v.is_empty()),
        tags: terms("tags"),
        genres: terms("genres"),
        release_year: if year.is_empty() {
            None
        } else {
            Some(
                year.parse()
                    .map_err(|_| ApiError::Unprocessable("Invalid release year".into()))?,
            )
        },
    };
    music_entities::update(&state, user.current.account.id, kind, id, &input).await?;
    Ok(settings::redirect_to(&format!(
        "/music/{}/{id}",
        kind.path()
    )))
}
pub async fn edit_artist(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    edit(state, user, Kind::Artist, id, body).await
}
pub async fn edit_album(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    edit(state, user, Kind::Album, id, body).await
}

fn create_key(kind: Kind) -> &'static str {
    match kind {
        Kind::Artist => "music-artist-create",
        Kind::Album => "music-album-create",
    }
}
async fn new_page(
    state: AppState,
    user: WebUser,
    kind: Kind,
    q: Page,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    let l = user.locale;
    let artist = if kind == Kind::Album {
        if let Some(id) = q.artist_id {
            let artist =
                music_entities::get(&state, Kind::Artist, id, Some(user.current.account.id))
                    .await?;
            if artist.owner_id != user.current.account.id {
                return Err(ApiError::NotFound);
            }
            Some(artist)
        } else {
            None
        }
    } else {
        None
    };
    let mut choices = if kind == Kind::Album && artist.is_none() {
        music_entities::catalogue(
            &state,
            Kind::Artist,
            Some(user.current.account.id),
            true,
            q.q.as_deref().unwrap_or_default(),
            q.max_id,
        )
        .await?
    } else {
        Vec::new()
    };
    let before = if choices.len() > 60 {
        choices.truncate(60);
        choices.last().map(|e| e.id)
    } else {
        None
    };
    let body = html! { section.column.music-library {
        h1{(l.text(create_key(kind)))}
        @if kind == Kind::Album && artist.is_none() {
            p{(l.text("music-album-choose-artist"))}
            form.settings-form method="get" action="/music/albums/new" {
                label for="music-entity-search"{(l.text("music-search"))}
                input id="music-entity-search" type="search" name="q" maxlength="200" value=(q.q.as_deref().unwrap_or_default());
                button type="submit"{(l.text("music-search-submit"))}
            }
            ul{@for choice in choices {li {a href=(format!("/music/albums/new?artist_id={}",choice.id)){(choice.name)}}}}
            @if let Some(before)=before {a href=(format!("/music/albums/new?max_id={before}&{}",serde_urlencoded::to_string([("q",q.q.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-older"))}}
            p{a href="/music/artists/new"{(l.text("music-artist-create"))}}
        } @else {
            p{(l.text(if kind==Kind::Album{"music-album-access-new-hint"}else{"music-entity-access-hint"}))}
            form.settings-form method="post" action=(format!("/web/music/{}/create",kind.path())) {
                input type="hidden" name="csrf" value=(user.csrf);
                @if let Some(artist)=artist {
                    input type="hidden" name="artist_id" value=(artist.id);
                    p{(l.text("music-artist")) ": "(artist.name)}
                }
                label for="entity-name"{(l.text("music-entity-name"))}
                input id="entity-name" name="name" required maxlength="200";
                label for="entity-description"{(l.text("music-entity-description"))}
                textarea id="entity-description" name="description" maxlength="4000"{}
                label for="entity-mbid"{(l.text("music-entity-mbid"))}
                input id="entity-mbid" name="musicbrainz_id" maxlength="36";
                label for="entity-tags"{(l.text("music-entity-tags"))}
                input id="entity-tags" name="tags";
                p{(l.text("music-entity-terms-hint"))}
                @if kind==Kind::Album {
                    label for="entity-genres"{(l.text("music-entity-genres"))}
                    input id="entity-genres" name="genres";
                    label for="entity-year"{(l.text("music-year"))}
                    input id="entity-year" name="release_year" type="number" min="1" max="9999";
                }
                button type="submit"{(l.text(create_key(kind)))}
            }
        }
        a href=(format!("/music/{}?mine=true",kind.path())){(l.text("music-entities-mine"))}
    }};
    Ok(response(&l.text(create_key(kind)), Some(&user), &body))
}
pub async fn new_artist(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    new_page(state, user, Kind::Artist, q).await
}
pub async fn new_album(
    State(state): State<AppState>,
    user: WebUser,
    Query(q): Query<Page>,
) -> Result<Response, ApiError> {
    new_page(state, user, Kind::Album, q).await
}
async fn create(
    state: AppState,
    user: WebUser,
    kind: Kind,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let field = |key| settings::field(&form, key).unwrap_or_default().to_owned();
    let terms = |key| {
        field(key)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let year = field("release_year");
    let input = music_entities::New {
        name: field("name"),
        description: field("description"),
        musicbrainz_id: Some(field("musicbrainz_id")).filter(|s| !s.is_empty()),
        tags: terms("tags"),
        genres: terms("genres"),
        release_year: if year.is_empty() {
            None
        } else {
            Some(
                year.parse()
                    .map_err(|_| ApiError::Unprocessable("Invalid release year".into()))?,
            )
        },
        artist_id: Some(field("artist_id")).filter(|s| !s.is_empty()),
    };
    let entity = music_entities::create(&state, user.current.account.id, kind, &input).await?;
    Ok(settings::redirect_to(&format!(
        "/music/{}/{}",
        kind.path(),
        entity.id
    )))
}
pub async fn create_artist(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    create(state, user, Kind::Artist, body).await
}
pub async fn create_album(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    create(state, user, Kind::Album, body).await
}

async fn delete(
    state: AppState,
    user: WebUser,
    kind: Kind,
    id: i64,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&form, "csrf") != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let input = music_entities::Delete {
        revision: settings::field(&form, "revision")
            .unwrap_or_default()
            .to_owned(),
        recursive: settings::field(&form, "recursive") == Some("true"),
    };
    music_entities::delete(&state, user.current.account.id, kind, id, &input).await?;
    Ok(settings::redirect_to(&format!(
        "/music/{}?mine=true",
        kind.path()
    )))
}
pub async fn delete_artist(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    delete(state, user, Kind::Artist, id, body).await
}
pub async fn delete_album(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    delete(state, user, Kind::Album, id, body).await
}
