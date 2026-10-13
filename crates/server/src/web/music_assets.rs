//! Server-rendered durable music library; every action works without JavaScript.
use super::{
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_assets::{self, Asset, Edit},
};
use axum::{
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::{account, music::Metadata};
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct QueryParams {
    pub q: Option<String>,
    pub mine: Option<bool>,
    pub max_id: Option<i64>,
    pub upload: Option<bool>,
    pub reused: Option<bool>,
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
fn upload_form(user: &WebUser) -> Markup {
    let l = user.locale;
    html! {
        p {(l.text("music-assets-upload-hint"))}
        form.settings-form.music-upload method="post" action="/web/music/assets/import" enctype="multipart/form-data" {
            input type="hidden" name="csrf" value=(user.csrf);
            label for="music-asset-file"{(l.text("music-assets-file"))}
            input id="music-asset-file" type="file" name="file" accept="audio/*" required;
            label for="music-upload-title"{(l.text("music-upload-title"))}
            input id="music-upload-title" name="title" maxlength="200";
            details {
                summary {(l.text("music-metadata-more"))}
                div.settings-form {
                    label for="music-upload-artist"{(l.text("music-artist"))}
                    input id="music-upload-artist" name="artist" maxlength="200";
                    label for="music-upload-album"{(l.text("music-album"))}
                    input id="music-upload-album" name="album" maxlength="200";
                }
            }
            p {(l.text("music-upload-publish-hint"))}
            div.music-publish-actions {
                button type="submit" name="operation" value="publish"{(l.text("music-upload-publish"))}
                button.button--quiet type="submit" name="operation" value="store"{(l.text("music-upload-save-private"))}
            }
        }
    }
}

pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(q): Query<QueryParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    if q.upload.unwrap_or(false)
        && let Some(user) = &user
    {
        let body = html! {section.column.music-library {
            h1 {(l.text("music-upload-audio"))}
            (super::music::workspace_navigation(l,true,"upload"))
            (upload_form(user))
            a href="/music/mine" {(l.text("music-mine-title"))}
        }};
        return Ok(response(&l.text("music-upload-audio"), Some(user), &body));
    }
    let mut tracks = music_assets::catalogue(
        &state,
        user.as_ref().map(|u| u.current.account.id),
        q.mine.unwrap_or(false),
        q.q.as_deref().unwrap_or_default(),
        q.max_id,
    )
    .await?;
    let before = if tracks.len() > 60 {
        tracks.truncate(60);
        tracks.last().map(|v| v.id)
    } else {
        None
    };
    let body = html! {section.column.music-library {
        h1{(l.text("music-assets-title"))}
        (super::music::workspace_navigation(l,user.is_some(),if q.upload.unwrap_or(false){"upload"}else{"tracks"}))
        @if user.is_some() {a.button href="/music/assets?mine=true&upload=true" {(l.text("music-upload-audio"))}}
        form.settings-form method="get" action="/music/assets"{
            @if q.mine.unwrap_or(false){input type="hidden" name="mine" value="true";}
            label for="music-assets-search"{(l.text("music-search"))}
            input id="music-assets-search" name="q" type="search" value=(q.q.as_deref().unwrap_or_default()) maxlength="200";
            button type="submit"{(l.text("music-search-submit"))}
        }
        @if !tracks.is_empty() && let Some(user)=&user {(super::music_downloads::selection_form(user))}
        @if tracks.is_empty(){p{(l.text("music-assets-empty"))}}
        ul{@for track in &tracks{li{@if let Some(user)=&user {(super::music_downloads::checkbox(user,"asset",track.id))}a href=(format!("/music/assets/{}",track.id)){(track.title)} " — " (track.artist)}}}
        @if let Some(before)=before{a href=(format!("/music/assets?mine={}&max_id={before}&{}",q.mine.unwrap_or(false),serde_urlencoded::to_string([("q",q.q.as_deref().unwrap_or_default())]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(
        &l.text("music-assets-title"),
        user.as_ref(),
        &body,
    ))
}
fn editor(asset: &Asset, user: &WebUser) -> Markup {
    let l = user.locale;
    html! {details{summary{(l.text("music-assets-edit"))}
        form.settings-form method="post" action=(format!("/web/music/assets/{}/edit",asset.id)){
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="revision" value=(asset.revision);
            label for="music-asset-title"{(l.text("compose-title"))}
            input id="music-asset-title" name="title" value=(asset.title) required maxlength="200";
            @for (key,label,value) in [("artist","music-artist",asset.artist.as_str()),("album","music-album",asset.album.as_str()),("genre","music-genre",asset.genre.as_str()),("musicbrainz_id","music-mbid",asset.musicbrainz_id.as_deref().unwrap_or_default())]{
                label for=(format!("music-asset-{key}")){(l.text(label))}
                input id=(format!("music-asset-{key}")) name=(key) value=(value) maxlength="200";
            }
            @for (key,label,value) in [("release_year","music-year",asset.release_year),("track_number","music-track-number",asset.track_number),("disc_number","music-disc-number",asset.disc_number)]{
                label for=(format!("music-asset-{key}")){(l.text(label))}
                input id=(format!("music-asset-{key}")) name=(key) type="number" min="1" max="9999" value=(value.map(|v|v.to_string()).unwrap_or_default());
            }
            label for="music-asset-visibility"{(l.text("music-assets-visibility"))}
            select id="music-asset-visibility" name="visibility"{
                @for (value,label) in [("private","music-assets-private"),("local","music-assets-local"),("public","music-public")]{option value=(value) selected[value==asset.visibility]{(l.text(label))}}
            }
            button type="submit"{(l.text("music-assets-save"))}
        }
    }}
}
async fn publication_url(
    state: &AppState,
    owner: i64,
    id: i64,
) -> Result<Option<String>, ApiError> {
    let publication = crate::music_publications::published(state, owner, id).await?;
    Ok(if let Some(status) = &publication {
        let author = account::find_by_id(&state.pool, status.account_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        Some(crate::entities::status_web_url(
            &state.config.domain,
            status,
            &author.username,
        ))
    } else {
        None
    })
}
async fn credits(
    state: &AppState,
    asset: &music_assets::Asset,
    viewer: Option<i64>,
) -> Result<maud::Markup, ApiError> {
    let readable_album = if let Some(id) = asset.album_id {
        match crate::music_entities::get(state, crate::music_entities::Kind::Album, id, viewer)
            .await
        {
            Ok(_) => Some(id),
            Err(ApiError::NotFound) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    Ok(html! {p{
        a href=(format!("/music/artists/{}",asset.artist_id)){(asset.artist)}
        @if asset.album_id.is_some(){" — " @if let Some(id)=readable_album{a href=(format!("/music/albums/{id}")){(asset.album)}}@else{(asset.album)}}
    }})
}
#[allow(
    clippy::too_many_lines,
    reason = "native resource page assembles its authorized controls and website cards"
)]
pub async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    Query(q): Query<QueryParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let asset = music_assets::get(&state, id, user.as_ref().map(|u| u.current.account.id)).await?;
    let credits = credits(
        &state,
        &asset,
        user.as_ref().map(|user| user.current.account.id),
    )
    .await?;
    let collected = super::music_saved::control(
        &state,
        user.as_ref(),
        crate::music_saved::Reference {
            kind: crate::music_saved::Kind::Track,
            id: id.to_string(),
        },
        user.as_ref()
            .is_some_and(|user| user.current.account.id == asset.owner_id),
        &format!("/music/assets/{id}"),
    )
    .await?;
    let saved = if let Some(user) = &user {
        crate::music_favourites::contains(
            &state,
            user.current.account.id,
            &crate::music_queues::Source::asset(id),
        )
        .await?
    } else {
        false
    };
    let artworks = crate::music_artworks::for_target(
        &state,
        crate::music_artworks::Target::Assets,
        id,
        user.as_ref().map(|u| u.current.account.id),
    )
    .await?;
    let subscription=if let Some((status,owner))=sqlx::query_as::<_,(i64,i64)>("SELECT p.status_id,t.owner_id FROM music_publications p JOIN music_assets t ON t.id=p.asset_id WHERE t.id=$1").bind(id).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)? {
        let item=plamenu_db::status::find_by_id(&state.pool,status).await?.ok_or(ApiError::NotFound)?;
        let owner=plamenu_db::account::find_by_id(&state.pool,owner).await?.ok_or(ApiError::NotFound)?;
        super::music::subscription(&state,user.as_ref(),&format!("{}/track",crate::entities::status_uri_for_account(&state.config.domain,&item,&owner)),&format!("/music/assets/{id}")).await?
    }else{html!{}};
    let publication_url = publication_url(&state, asset.owner_id, id).await?;
    let l = user
        .as_ref()
        .map_or_else(|| super::i18n::Locale::from_headers(&headers), |u| u.locale);
    let body = html! {section.column.music-library {
        h1{(asset.title)}
        @if q.reused.unwrap_or(false) && user.as_ref().is_some_and(|user|user.current.account.id==asset.owner_id){p role="status"{(l.text("music-upload-reused"))}}
        (super::music::workspace_navigation(l,user.is_some(),"tracks"))
        (super::music_artworks::images(l,&asset.title,&artworks))
        (collected)
        @if artworks.is_empty(){@if let Some(art)=asset.artwork_id{img.music-artwork src=(format!("/music/artwork/{art}")) width=(asset.artwork_width.unwrap_or(1)) height=(asset.artwork_height.unwrap_or(1)) alt=(format!("{}: {}",asset.title,l.text("music-artwork-cover")));}}
        (credits)
        dl.status__music {
            @if !asset.genre.is_empty(){dt{(l.text("music-genre"))}dd{(super::music::term_links(l,crate::music_feeds::Term::Genre,&asset.genre.split([';',',','|']).map(str::trim).filter(|term|!term.is_empty()).map(str::to_owned).collect::<Vec<_>>()))}}
            @for (label,value) in [("music-year",asset.release_year),("music-track-number",asset.track_number),("music-disc-number",asset.disc_number)]{@if let Some(value)=value{dt{(l.text(label))}dd{(value)}}}
            @if let Some(mbid)=&asset.musicbrainz_id{dt{(l.text("music-mbid"))}dd{(mbid)}}
        }
        @if !asset.description.is_empty(){(super::music_track_details::description(&asset))}
        @if !asset.tags.is_empty(){p{(l.text("music-entity-tags")) ": "(super::music::term_links(l,crate::music_feeds::Term::Tag,&asset.tags))}}
        @if !asset.extra_artists.is_empty(){p{(l.text("music-track-credits")) ": "(asset.extra_artists.join(", "))}}
        @if user.as_ref().is_some_and(|user|user.current.account.id==asset.owner_id){
            p{(l.text(if publication_url.is_some(){"music-upload-published"}else if asset.visibility=="private"{"music-upload-private"}else{"music-upload-unpublished"}))}
            p{(l.text(match asset.visibility.as_str(){"public"=>"music-public","local"=>"music-assets-local",_=>"music-assets-private"}))}
        }
        audio controls preload="metadata" aria-label=(asset.title) src=(format!("/music/assets/{id}/stream")){}
        @if let Some(user)=&user{(super::music_player::queue_link(user,"asset",id))}
        @if let Some(user)=&user{(super::music_favourites::form(l,&user.csrf,Some(&id.to_string()),None,saved,&format!("/music/assets/{id}")))}
        @if let Some(url)=&publication_url {p{a href=(url){(l.text("music-assets-view-publication"))}}}
        @if let Some(user)=&user { @if user.current.account.id==asset.owner_id {
            @if publication_url.is_none() && asset.visibility=="public"{
                form method="post" action=(format!("/web/music/assets/{id}/publish")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    button type="submit"{(l.text("music-assets-publish"))}
                }
            }

            details.music-management {summary {(l.text("music-manage-track"))}
            (super::music_artworks::forms(user,crate::music_artworks::Target::Assets,id,asset.revision,&artworks))
            (editor(&asset,user))
            (super::music_track_details::editor(&asset,user))
            details{summary{(l.text("music-assets-share"))}
                form.settings-form method="post" action=(format!("/web/music/assets/{id}/grant")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    label for="music-share-member"{(l.text("music-assets-member"))}
                    input id="music-share-member" name="member" required maxlength="200";
                    button type="submit" name="operation" value="grant"{(l.text("music-assets-grant"))}
                    button type="submit" name="operation" value="revoke"{(l.text("music-assets-revoke"))}
                }
            }
            }
        }}
        details{summary{(l.text("music-resource-tools"))}
        @if let Some(user)=&user{
            (super::music_downloads::link(user,"asset",id))
            @if user.current.account.id==asset.owner_id{
            @if publication_url.is_some(){
                form method="post" action=(format!("/web/music/assets/{id}/withdraw")){
                    input type="hidden" name="csrf" value=(user.csrf);
                    button type="submit"{(l.text("music-assets-withdraw"))}
                }
            }
            form method="post" action=(format!("/web/music/assets/{id}/delete")){
                input type="hidden" name="csrf" value=(user.csrf);
                button.settings-button--danger type="submit"{(l.text("music-assets-delete"))}
            }
            }
        }
        @if asset.visibility=="public"{(super::music_embeds::link(l,"assets",id))}
        (super::music_activity::link(l,"assets",asset.id))
        (subscription)
        (super::webmentions::for_target(
        &state,
        &format!("https://{}/music/assets/{id}", state.config.domain),
        l,
    )
    .await?)
        p{a href=(format!("/music/assets/{id}/download")){(l.text("music-assets-download"))}}
        p{a href=(format!("/music/assets/{id}/renditions")){(l.text("music-renditions-title"))}}
        }
        p{a href="/music/mine"{(l.text("music-mine-title"))}}
    }};
    Ok(response(&asset.title, user.as_ref(), &body))
}
pub async fn import(
    State(state): State<AppState>,
    user: WebUser,
    mut form: Multipart,
) -> Result<Response, ApiError> {
    let mut csrf = None;
    let mut upload = None;
    let mut fields = Vec::new();
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::BadRequest("Invalid audio upload".into()))?
    {
        match field.name() {
            Some("csrf") => csrf = Some(field.text().await.unwrap_or_default()),
            Some("file") => {
                if upload.is_some() {
                    return Err(ApiError::Unprocessable("Upload one track at a time".into()));
                }
                let name = field.file_name().unwrap_or("Untitled track").to_owned();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| ApiError::BadRequest("Invalid audio upload".into()))?;
                upload = Some((name, bytes.to_vec()));
            }
            Some(name @ ("title" | "artist" | "album" | "operation")) => {
                let name = name.to_owned();
                fields.push((name, field.text().await.unwrap_or_default()));
            }
            _ => {}
        }
    }
    if csrf.as_deref() != Some(user.csrf.as_str()) {
        return Ok(super::session::csrf_rejection());
    }
    let (name, bytes) =
        upload.ok_or_else(|| ApiError::Unprocessable("Choose an audio file".into()))?;
    let publish = match settings::field(&fields, "operation") {
        None | Some("store") => false,
        Some("publish") => true,
        _ => return Err(ApiError::Unprocessable("Choose upload or publish".into())),
    };
    if publish {
        user.current.require_scope("write:statuses")?;
    }
    for (key, value) in &fields {
        if key != "operation" && (value.chars().count() > 200 || value.contains('\0')) {
            return Err(ApiError::Unprocessable(
                "Audio title and credits must be at most 200 characters".into(),
            ));
        }
    }
    let mut asset = match music_assets::import(&state, user.current.account.id, bytes, &name).await
    {
        Ok(asset) => asset,
        Err(error) => return upload_problem(&user, error, None),
    };
    if !publish && asset.visibility != "private" {
        return Ok(settings::redirect_to(&format!(
            "/music/assets/{}?reused=true",
            asset.id
        )));
    }
    if let Some(input) = upload_edit(&asset, &fields, publish) {
        asset = music_assets::update(&state, user.current.account.id, asset.id, &input).await?;
    }
    if publish
        && let Err(error) =
            crate::music_publications::publish(&state, &user.current.account, asset.id).await
    {
        return upload_problem(&user, error, Some(asset.id));
    }
    Ok(settings::redirect_to(&format!(
        "/music/assets/{}",
        asset.id
    )))
}
fn upload_edit(asset: &Asset, fields: &[(String, String)], publish: bool) -> Option<Edit> {
    let mut music = asset.metadata();
    let title = settings::field(fields, "title")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&asset.title)
        .to_owned();
    if let Some(artist) = settings::field(fields, "artist").filter(|value| !value.trim().is_empty())
    {
        music.artist = artist.trim().into();
    }
    if let Some(album) = settings::field(fields, "album").filter(|value| !value.trim().is_empty()) {
        music.album = album.trim().into();
    }
    (publish || title != asset.title || music != asset.metadata()).then(|| Edit {
        title,
        visibility: if publish { "public" } else { "private" }.into(),
        revision: Some(asset.revision.to_string()),
        music,
    })
}
fn pairs(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(pairs)
}
pub async fn edit(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = pairs(&user, &body)?;
    let field = |key| settings::field(&form, key).unwrap_or_default().to_owned();
    let number = |key| -> Result<Option<i32>, ApiError> {
        let v = field(key);
        if v.is_empty() {
            Ok(None)
        } else {
            v.parse()
                .map(Some)
                .map_err(|_| ApiError::Unprocessable("Invalid music number".into()))
        }
    };
    let input = Edit {
        title: field("title"),
        visibility: field("visibility"),
        revision: Some(field("revision")),
        music: Metadata {
            artist: field("artist"),
            album: field("album"),
            genre: field("genre"),
            release_year: number("release_year")?,
            track_number: number("track_number")?,
            disc_number: number("disc_number")?,
            musicbrainz_id: Some(field("musicbrainz_id")).filter(|v| !v.is_empty()),
        },
    };
    music_assets::update(&state, user.current.account.id, id, &input).await?;
    Ok(settings::redirect_to(&format!("/music/assets/{id}")))
}
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    pairs(&user, &body)?;
    music_assets::delete(&state, user.current.account.id, id).await?;
    Ok(settings::redirect_to("/music/assets?mine=true"))
}
pub async fn grant(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let form = pairs(&user, &body)?;
    let name = settings::field(&form, "member").unwrap_or_default();
    let target = account::find_local_by_username(&state.pool, name)
        .await?
        .ok_or(ApiError::NotFound)?;
    let enabled = match settings::field(&form, "operation") {
        Some("grant") => true,
        Some("revoke") => false,
        _ => return Err(ApiError::Unprocessable("Choose grant or revoke".into())),
    };
    music_assets::grant(&state, user.current.account.id, id, target.id, enabled).await?;
    Ok(settings::redirect_to(&format!("/music/assets/{id}")))
}

pub async fn publish(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    pairs(&user, &body)?;
    user.current.require_scope("write:statuses")?;
    let status = crate::music_publications::publish(&state, &user.current.account, id).await?;
    Ok(settings::redirect_to(&crate::entities::status_web_url(
        &state.config.domain,
        &status,
        &user.current.account.username,
    )))
}
pub async fn withdraw(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    pairs(&user, &body)?;
    user.current.require_scope("write:statuses")?;
    crate::music_publications::withdraw(&state, &user.current.account, id).await?;
    Ok(settings::redirect_to(&format!("/music/assets/{id}")))
}

fn upload_problem(
    user: &WebUser,
    error: ApiError,
    asset: Option<i64>,
) -> Result<Response, ApiError> {
    let l = user.locale;
    tracing::warn!(owner=user.current.account.id,error=%error.chain(),"Music upload or publication could not finish");
    let status = if asset.is_some() {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    } else {
        match error {
            ApiError::BadRequest(_) | ApiError::Unprocessable(_) => {
                axum::http::StatusCode::UNPROCESSABLE_ENTITY
            }
            other => return Err(other),
        }
    };
    let body = html! {section.column{h1{(l.text("music-upload-problem"))}p role="alert"{(l.text(if asset.is_some(){"music-upload-publication-problem"}else{"music-upload-file-problem"}))}
        @if let Some(id)=asset{a href=(format!("/music/assets/{id}")){(l.text("music-upload-open-saved"))}}
        @else {a href="/music/assets?mine=true&upload=true"{(l.text("music-upload-audio"))}}
    }};
    Ok((
        status,
        [(header::CACHE_CONTROL, "private, no-store")],
        layout::shell(&l.text("music-upload-problem"), Some(user), &body),
    )
        .into_response())
}
