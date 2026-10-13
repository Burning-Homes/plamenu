//! Streams are a child page of the single Music workspace.
use super::{
    layout,
    session::{MaybeWebUser, WebUser},
    settings,
};
use crate::{
    AppState,
    error::ApiError,
    music_outputs::{self, Command, New, Output},
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
#[derive(Default, serde::Deserialize)]
pub struct Page {
    #[serde(default)]
    pub mine: bool,
    pub before: Option<i64>,
    pub remote_before: Option<i64>,
    #[serde(default)]
    pub q: String,
}
fn response(title: &str, user: Option<&WebUser>, body: &Markup) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        layout::shell(title, user, body),
    )
        .into_response()
}
fn fields(l: super::i18n::Locale, row: Option<&Output>) -> Markup {
    html! {
        label for="stream-name"{(l.text("music-stream-name"))} input id="stream-name" name="name" required maxlength="200" value=(row.map_or("",|row|row.name.as_str()));
        label for="stream-mount"{(l.text("music-stream-mount"))} input id="stream-mount" name="mount" required maxlength="64" pattern="[A-Za-z0-9][A-Za-z0-9._-]{0,63}" value=(row.map_or("",|row|row.mount.as_str()));
        label for="stream-description"{(l.text("music-collection-description"))}textarea id="stream-description" name="description" maxlength="4000"{(row.map_or("",|row|row.description.as_str()))}
        label for="stream-genre"{(l.text("music-genre"))}input id="stream-genre" name="genre" maxlength="200" value=(row.map_or("",|row|row.genre.as_str()));
        label for="stream-format"{(l.text("music-stream-format"))}select id="stream-format" name="format"{@for format in ["mp3","ogg","opus"]{option value=(format) selected[row.map_or(format=="mp3",|row|row.format==format)]{(format.to_uppercase())}}}
        label for="stream-bitrate"{(l.text("music-stream-bitrate"))}input id="stream-bitrate" name="bitrate" type="number" min="32" max="512" required value=(row.map_or(128,|row|row.bitrate));
        label for="stream-rate"{(l.text("music-stream-rate"))}select id="stream-rate" name="sample_rate"{@for rate in [22050,24000,32000,44100,48000]{option value=(rate) selected[row.map_or(rate==44100,|row|row.sample_rate==rate)]{(rate)}}}
        label for="stream-visibility"{(l.text("music-assets-visibility"))}select id="stream-visibility" name="visibility"{@for(value,key)in[("private","music-assets-private"),("public","music-public")]{option value=(value) selected[row.map_or(value=="private",|row|row.visibility==value)]{(l.text(key))}}}
    }
}
pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(page): Query<Page>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let l = user.as_ref().map_or_else(
        || super::i18n::Locale::from_headers(&headers),
        |user| user.locale,
    );
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
    let mut rows = music_outputs::list(
        &state,
        user.as_ref().map(|user| user.current.account.id),
        page.mine,
        page.before,
        &page.q,
    )
    .await?;
    let before = if rows.len() > 50 {
        rows.truncate(50);
        rows.last().map(|row| row.id)
    } else {
        None
    };
    let title = l.text("music-streams-title");
    let remote = if page.mine {
        html! {}
    } else {
        super::music_browse::remote_section(
            &state,
            user.as_ref(),
            l,
            &page.q,
            page.remote_before,
            "live",
        )
        .await?
    };
    let sites = super::music_browse::source_sites(&state, user.as_ref(), l, "streams").await?;
    let body = html! {section.column.music-library {h1{(title)}(super::music::workspace_navigation(l,user.is_some(),"streams"))
        nav.video-explore__links{a href="/music/streams"{(l.text("music-stations-all"))}a href="/music/radios"{(l.text("music-stations-saved"))}@if user.is_some(){a href="/music/streams?mine=true"{(l.text("music-streams-mine"))}}}
        form.music-library-filter method="get" action="/music/streams"{input type="hidden" name="mine" value=(page.mine);label.visually-hidden for="stream-search"{(l.text("music-stations-search"))}input id="stream-search" placeholder=(l.text("music-stations-search")) type="search" name="q" maxlength="200" value=(page.q);button{(l.text("music-search-submit"))}}
        @if let Some(user)=&user {details{summary{(l.text("music-stream-create"))}form.settings-form method="post" action="/web/music/streams/create"{input type="hidden" name="csrf" value=(user.csrf);(fields(l,None))button{(l.text("music-stream-create"))}}}}
        @if rows.is_empty() && remote.0.is_empty(){p{(l.text("music-streams-empty"))}}
        ul{@for row in &rows{li{a href=(format!("/music/streams/{}",row.id)){(row.name)}" · "(l.text(if row.transport=="playing"{"music-stream-playing"}else{"music-stream-paused"}))}}}
        (remote)(sites)
        @if let Some(before)=before{a href=(format!("/music/streams?mine={}&before={before}&{}",page.mine,serde_urlencoded::to_string([("q",page.q.as_str())]).unwrap_or_default())){(l.text("music-older"))}}
    }};
    Ok(response(&title, user.as_ref(), &body))
}
pub async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeWebUser(user): MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let viewer = user.as_ref().map(|user| user.current.account.id);
    let row = music_outputs::get(&state, id, viewer).await?;
    let l = user.as_ref().map_or_else(
        || super::i18n::Locale::from_headers(&headers),
        |user| user.locale,
    );
    let body = html! {section.column.music-library {h1{(row.name)}(super::music::workspace_navigation(l,user.is_some(),"streams"))
        p{(row.description)} @if !row.now_playing.is_empty(){p role="status"{(row.now_playing)}} @if !row.genre.is_empty(){p{(row.genre)}}
        p{(row.format.to_uppercase())" · "(row.bitrate)" kbps · "(row.sample_rate)" Hz"}
        @if row.enabled && row.transport=="playing"{audio controls preload="none" src=(format!("/music/live/{}",row.mount)) aria-label=(row.name) {}}
        @if let Some(user)=&user {@if viewer==Some(row.owner_id){(owner_controls(user,&row))}}
    }};
    Ok(response(&row.name, user.as_ref(), &body))
}
pub(super) fn owner_controls(user: &WebUser, row: &Output) -> Markup {
    let l = user.locale;
    let id = row.id;
    html! {p{a href=(format!("/music/player?device={}",row.device)){(l.text("music-stream-queue"))}}
        @if let Some(error)=&row.last_error{p.settings__error role="status"{(error)}}
        form.settings-form method="post" action=(format!("/web/music/streams/{id}/command")){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);
            @for(operation,key)in[("play","music-stream-play"),("pause","music-stream-pause"),("stop","music-stream-stop"),(if row.enabled{"disable"}else{"enable"},if row.enabled{"music-stream-disable"}else{"music-stream-enable"})]{button name="operation" value=(operation){(l.text(key))}}}
        details{summary{(l.text("music-stream-settings"))}form.settings-form method="post" action=(format!("/web/music/streams/{id}/update")){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);(fields(l,Some(row)))button{(l.text("music-stream-save"))}}}
        @if row.visibility=="private"{form method="post" action=(format!("/web/music/streams/{id}/token")){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);button{(l.text("music-stream-rotate"))}}}
        form method="post" action=(format!("/web/music/streams/{id}/delete")){input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(row.revision);button{(l.text("music-stream-delete"))}}
    }
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:statuses")?;
    let fields = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&fields, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(fields)
}
fn revision(fields: &[(String, String)]) -> Result<i64, ApiError> {
    settings::field(fields, "revision")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ApiError::Unprocessable("Invalid HTTP stream revision".into()))
}
fn input(fields: &[(String, String)]) -> Result<New, ApiError> {
    let value = |key| settings::field(fields, key).unwrap_or_default();
    Ok(New {
        mount: value("mount").into(),
        name: value("name").into(),
        description: value("description").into(),
        genre: value("genre").into(),
        format: value("format").into(),
        bitrate: Some(
            value("bitrate")
                .parse()
                .map_err(|_| ApiError::Unprocessable("Invalid stream bitrate".into()))?,
        ),
        sample_rate: value("sample_rate")
            .parse()
            .map_err(|_| ApiError::Unprocessable("Invalid sample rate".into()))?,
        visibility: value("visibility").into(),
    })
}
fn created(user: &WebUser, result: music_outputs::Created) -> Response {
    let id = result.output["id"].as_str().unwrap_or_default();
    if let Some(token) = result.listen_token {
        let title = user.locale.text("music-stream-token-title");
        let link = format!("/music/streams/{id}/audio?token={token}");
        let body = html! {section.column.music-library{h1{(title)}p{(user.locale.text("music-stream-token-hint"))}a href=(link) rel="noreferrer"{(user.locale.text("music-stream-listen-link"))}p{a href=(format!("/music/streams/{id}")){(user.locale.text("music-stream-open"))}}}};
        response(&title, Some(user), &body)
    } else {
        settings::redirect_to(&format!("/music/streams/{id}"))
    }
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = form(&user, &body)?;
    Ok(created(
        &user,
        music_outputs::create(&state, user.current.account.id, &input(&fields)?).await?,
    ))
}
pub async fn update(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = form(&user, &body)?;
    Ok(created(
        &user,
        music_outputs::update(
            &state,
            user.current.account.id,
            id,
            revision(&fields)?,
            &input(&fields)?,
        )
        .await?,
    ))
}
pub async fn command(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = form(&user, &body)?;
    music_outputs::command(
        &state,
        user.current.account.id,
        id,
        &Command {
            revision: revision(&fields)?.to_string(),
            operation: settings::field(&fields, "operation")
                .unwrap_or_default()
                .into(),
        },
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/streams/{id}")))
}
pub async fn rotate(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = form(&user, &body)?;
    Ok(created(
        &user,
        music_outputs::rotate_token(&state, user.current.account.id, id, revision(&fields)?)
            .await?,
    ))
}
pub async fn delete(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let fields = form(&user, &body)?;
    music_outputs::delete(&state, user.current.account.id, id, revision(&fields)?).await?;
    Ok(settings::redirect_to("/music/streams?mine=true"))
}
