//! Native forms remain the complete workflow; scripting enhances clip playback.
use super::{
    i18n::Locale,
    layout, pages,
    session::{MaybeWebUser, WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
    view,
};
use crate::{AppState, error::ApiError, video_playlists as service};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::video_playlists as db;
use serde::Deserialize;

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}
#[derive(Default, Deserialize)]
pub struct QueryParams {
    mine: Option<bool>,
    max_id: Option<i64>,
    entry: Option<i64>,
    page: Option<usize>,
}
fn profile_fields(p: Option<&db::Playlist>, locale: Locale) -> Markup {
    html! {
        label.settings-field {span.settings-field__label {(locale.text("playlist-name"))} input name="name" required maxlength="120" value=(p.map_or("",|p|p.name.as_str()));}
        label.settings-field {span.settings-field__label {(locale.text("playlist-description"))} textarea name="description" maxlength="4000" rows="3" {(p.map_or("",|p|p.description.as_str()))}}
        label.settings-field {span.settings-field__label {(locale.text("playlist-visibility"))} select name="visibility" {
            @for value in ["private","unlisted","public"] {option value=(value) selected[p.map_or("private",|p|p.visibility.as_str())==value] {(locale.text(&format!("playlist-{value}")))}}
        }}
        p.settings-field__hint {(locale.text("playlist-privacy-hint"))}
    }
}
fn hidden(p: &db::Playlist, user: &WebUser, op: &str) -> Markup {
    html! {
        input type="hidden" name="csrf" value=(user.csrf);
        input type="hidden" name="revision" value=(p.revision);
        input type="hidden" name="operation" value=(op);
    }
}
fn clip_fields(start: i32, stop: Option<i32>, locale: Locale) -> Markup {
    html! {
        label.settings-field {span.settings-field__label {(locale.text("playlist-start"))} input type="number" name="start_seconds" min="0" max="604800" step="1" value=(start);}
        label.settings-field {span.settings-field__label {(locale.text("playlist-stop"))} input type="number" name="stop_seconds" min="1" max="604800" step="1" value=(stop.map(|s|s.to_string()).unwrap_or_default());}
        p.settings-field__hint {(locale.text("playlist-clip-hint"))}
    }
}

pub async fn index(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Query(q): Query<QueryParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    render_index(
        &state,
        user.as_ref(),
        Locale::from_headers(&headers),
        &q,
        None,
    )
    .await
}
async fn render_index(
    state: &AppState,
    user: Option<&WebUser>,
    request_locale: Locale,
    q: &QueryParams,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let locale = user.map_or(request_locale, |u| u.locale);
    let viewer = user.map(|u| u.current.account.id);
    let mut rows = service::list(state, viewer, q.mine.unwrap_or(false), q.max_id).await?;
    let more = rows.len() > 20;
    rows.truncate(20);
    let channels = if let Some(viewer) = viewer {
        plamenu_db::video_channel_access::available(&state.pool, viewer).await?
    } else {
        Vec::new()
    };
    let accounts = plamenu_db::account::find_by_ids(
        &state.pool,
        &channels.iter().map(|c| c.account_id).collect::<Vec<_>>(),
    )
    .await?;
    let body = html! {section.column.video-playlists.video-workspace {
        h1 {(locale.text("playlist-title"))}
        @if user.is_some(){(super::video_workspace::navigation("/videos/playlists?mine=true",locale))}
        nav.video-explore__links aria-label=(locale.text("playlist-navigation")) {
            a href="/videos/explore" {(locale.text("videos-explore-browse"))}
            a href="/videos/playlists" {(locale.text("playlist-public-library"))}
            @if user.is_some(){a href="/videos/playlists?mine=true" {(locale.text("playlist-my-library"))}}
        }
        @if let Some(error)=error {p.settings__error role="alert" {(error)}}
        @if user.is_some(){details.video-workspace__create open[error.is_some() || rows.is_empty()] {
            summary {(locale.text("playlist-create"))}
        @if let Some(user)=user {
            @if accounts.is_empty(){p {a href="/videos/channels/new" {(locale.text("videos-create-channel"))}}}
            @else {form.settings-form method="post" action="/web/video-playlists" {
                input type="hidden" name="csrf" value=(user.csrf);
                label.settings-field {span.settings-field__label {(locale.text("playlist-channel"))} select name="channel_id" required {@for a in &accounts {option value=(a.id) {(a.display_name)}}}}
                (profile_fields(None,locale)) button type="submit" {(locale.text("playlist-create"))}
            }}
            h2 {(locale.text("playlist-import"))}
            p.settings-field__hint {(locale.text("playlist-import-hint"))}
            form.settings-form method="post" action="/web/video-playlists/import" {
                input type="hidden" name="csrf" value=(user.csrf);
                label.settings-field {span.settings-field__label {(locale.text("playlist-url"))} input type="url" name="url" required maxlength="2048";}
                button type="submit" {(locale.text("playlist-import"))}
            }
        }
        }}
        ul.video-playlists__library.video-workspace__library {@for p in &rows {li.video-workspace__row {a.video-workspace__title href=(format!("/videos/playlists/{}",p.id)) {(p.name)} " — " (locale.text(&format!("playlist-{}",p.visibility)))}}}
        @if rows.is_empty(){p {(locale.text("playlist-empty"))}}
        @if more && let Some(last)=rows.last(){a href=(format!("/videos/playlists?mine={}&max_id={}",q.mine.unwrap_or(false),last.id)) {(locale.text("playlist-older"))}}
    }};
    Ok(private(
        layout::shell(&locale.text("playlist-title"), user, &body).into_response(),
    ))
}

pub async fn show(
    State(state): State<AppState>,
    MaybeWebUser(user): MaybeWebUser,
    Path(id): Path<i64>,
    Query(q): Query<QueryParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    render(
        &state,
        user.as_ref(),
        Locale::from_headers(&headers),
        id,
        &q,
        None,
    )
    .await
}
#[allow(
    clippy::too_many_lines,
    reason = "HTML-first playlist playback and editing template"
)]
async fn render(
    state: &AppState,
    user: Option<&WebUser>,
    request_locale: Locale,
    id: i64,
    q: &QueryParams,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let viewer = user.map(|u| u.current.account.id);
    let locale = user.map_or(request_locale, |u| u.locale);
    let p = service::readable(state, id, viewer).await?;
    let entries = db::entries(&state.pool, p.id).await?;
    let editable = if let Some(viewer) = viewer {
        service::can_edit(state, &p, viewer).await?
    } else {
        false
    };
    let entity = crate::routes::video_playlists::entity(state, &p, viewer).await?;
    let selected = q
        .entry
        .and_then(|id| entries.iter().find(|e| e.id == id))
        .or_else(|| {
            if q.entry.is_none() {
                entries.first()
            } else {
                None
            }
        });
    if q.entry.is_some() && selected.is_none() {
        return Err(ApiError::NotFound);
    }
    let stored = if let Some(entry) = selected {
        crate::video_playlists_remote::resolve_entry(state, &p, entry, viewer).await?
    } else {
        None
    };
    let mut video = if let Some(stored) = stored.as_ref() {
        Some(
            crate::entities::render_status(&state.pool, &state.config.domain, stored, viewer)
                .await?,
        )
    } else {
        None
    };
    if let (Some(entity), Some(entry)) = (&mut video, selected)
        && let Some(media) = entity["media_attachments"].as_array_mut()
    {
        for item in media {
            if let Some(url) = item["url"].as_str() {
                item["url"] = serde_json::json!(format!(
                    "{}#t={}{}",
                    url.split('#').next().unwrap_or(url),
                    entry.start_seconds,
                    entry
                        .stop_seconds
                        .map(|end| format!(",{end}"))
                        .unwrap_or_default()
                ));
            }
        }
    }
    let back = format!("/videos/playlists/{id}");
    let viewer_string = viewer.map(|id| id.to_string());
    let ctx = view::Ctx {
        csrf: user.map(|u| u.csrf.as_str()),
        viewer_id: viewer_string.as_deref(),
        return_to: &back,
        filter_context: None,
        prefs: pages::view_prefs(state, user).await?,
        locale,
        clock: pages::clock_for(user, locale),
        admin: user.map(WebUser::admin_capabilities).unwrap_or_default(),
    };
    let page = q.page.unwrap_or(1).max(1);
    let begin = page.saturating_sub(1).saturating_mul(25);
    let body = html! {section.column.video-playlist {
        h1 {(&p.name)}
        nav.video-explore__links aria-label=(locale.text("playlist-navigation")){a href="/videos/playlists" {(locale.text("playlist-title"))} @if user.is_some(){a href="/videos/playlists?mine=true" {(locale.text("playlist-my-library"))}}}
        p {(locale.text(&format!("playlist-{}",p.visibility)))} p.video-playlist__description {(&p.description)}
        @if let Some(error)=error{p.settings__error role="alert" {(error)}}
        @if p.remote_uri.is_some(){p {(locale.text("playlist-remote-hint"))} @if let Some(user)=user{form method="post" action="/web/video-playlists/import" {input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="url" value=(service::uri(state,&p));button type="submit" {(locale.text("playlist-refresh"))}}}}
        @if let Some(entry)=selected {
            section.video-playlist__player data-playlist-start=(entry.start_seconds) data-playlist-stop=(entry.stop_seconds.map(|v|v.to_string()).unwrap_or_default()) data-playlist-ended=(locale.text("playlist-clip-ended")) {
                h2 {(locale.text("playlist-now-playing")) " " (entry.position)}
                @if entry.start_seconds>0 || entry.stop_seconds.is_some(){p {(locale.text("playlist-clip-range")) " " (entry.start_seconds) "–" (entry.stop_seconds.map_or_else(||locale.text("playlist-video-end"),|v|v.to_string())) " s"}}
                @if let Some(video)=&video{(view::status_card(&view::Status(video),&ctx))}
                @else {p role="status" {(locale.text("playlist-unavailable"))}}
                p data-playlist-status role="status" aria-live="polite" {}
                nav.video-playlist__next aria-label=(locale.text("playlist-playback-navigation")) {
                    @if let Some(previous)=entries.iter().rev().find(|e|e.position<entry.position){a href=(format!("{back}?entry={}",previous.id)) {(locale.text("playlist-previous"))}}
                    @if let Some(next)=entries.iter().find(|e|e.position>entry.position){a href=(format!("{back}?entry={}",next.id)) {(locale.text("playlist-next"))}}
                }
            }
        }
        h2 {(locale.text("playlist-entries"))}
        p.settings-field__hint {(locale.text("playlist-duplicates-hint"))}
        ol.video-playlist__entries start=(begin+1) {
            @for (entry,item) in entries.iter().zip(entity["entries"].as_array().unwrap()).skip(begin).take(25) {
                li.video-playlist__entry id=(format!("entry-{}",entry.id)) {
                    a href=(format!("{back}?entry={}&page={page}",entry.id)) aria-current=(if Some(entry.id)==selected.map(|e|e.id){"true"}else{"false"}) {
                        (item["name"].as_str().unwrap_or(""))
                        @if item["name"].as_str().is_none(){(locale.text(if item["unresolved"]==true{"playlist-unresolved"}else{"playlist-unavailable"}))}
                    }
                    p {(locale.text("playlist-clip-range")) " " (entry.start_seconds) "–" (entry.stop_seconds.map_or_else(||locale.text("playlist-video-end"),|v|v.to_string())) " s"}
                    @if editable && let Some(user)=user {
                        details {summary {(locale.text("playlist-edit-entry")) " " (entry.position)}
                            form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) data-playlist-move {
                                (hidden(&p,user,"move")) input type="hidden" name="entry_id" value=(entry.id);
                                label.settings-field {span.settings-field__label {(locale.text("playlist-position"))} input type="number" name="position" min="1" max=(entries.len()) value=(entry.position) required;}
                                button type="submit" {(locale.text("playlist-move"))}
                                div.video-playlist__next {
                                    button type="button" data-playlist-delta="-1" hidden {(locale.text("playlist-move-up"))}
                                    button type="button" data-playlist-delta="1" hidden {(locale.text("playlist-move-down"))}
                                }
                            }
                            form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) {
                                (hidden(&p,user,"clip")) input type="hidden" name="entry_id" value=(entry.id);
                                (clip_fields(entry.start_seconds,entry.stop_seconds,locale)) button type="submit" {(locale.text("playlist-save-clip"))}
                            }
                            form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) {
                                (hidden(&p,user,"remove")) input type="hidden" name="entry_id" value=(entry.id);
                                button type="submit" {(locale.text("playlist-remove"))}
                            }
                        }
                    }
                }
            }
        }
        @if begin>0 {a href=(format!("{back}?page={}",page-1)) {(locale.text("playlist-previous-page"))}}
        @if begin+25<entries.len(){a href=(format!("{back}?page={}",page+1)) {(locale.text("playlist-next-page"))}}
        @if editable && let Some(user)=user {
            h2 {(locale.text("playlist-add"))}
            form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) {
                (hidden(&p,user,"add"))
                label.settings-field {span.settings-field__label {(locale.text("playlist-video-url"))} input type="url" name="video_url" required maxlength="2048";}
                (clip_fields(0,None,locale)) button type="submit" {(locale.text("playlist-add"))}
            }
            details {summary {(locale.text("playlist-settings"))}
                form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) {
                    (hidden(&p,user,"profile")) (profile_fields(Some(&p),locale)) button type="submit" {(locale.text("playlist-save"))}
                }
                h2 {(locale.text("playlist-delete"))} p {(locale.text("playlist-delete-hint"))}
                form.settings-form method="post" action=(format!("/web/video-playlists/{id}")) {
                    (hidden(&p,user,"delete"))
                    label.settings-field {span.settings-field__label {(locale.text("playlist-confirm"))} input name="confirm" required autocomplete="off";}
                    button.settings__danger type="submit" {(locale.text("playlist-delete"))}
                }
            }
        }
    }};
    Ok(private(layout::shell(&p.name, user, &body).into_response()))
}

fn parse(body: &Bytes) -> Result<(Vec<(String, String)>, service::Change), ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::Unprocessable)?;
    let text = |key| field(&pairs, key).map(str::to_owned);
    let number = |key| -> Result<Option<i64>, ApiError> {
        field(&pairs, key)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse().map_err(|_| {
                    ApiError::Unprocessable("Use whole seconds and whole entry positions".into())
                })
            })
            .transpose()
    };
    let small = |key| -> Result<Option<i32>, ApiError> {
        number(key)?
            .map(|v| {
                i32::try_from(v)
                    .map_err(|_| ApiError::Unprocessable("Number exceeds its limit".into()))
            })
            .transpose()
    };
    let input = service::Change {
        operation: text("operation").unwrap_or_default(),
        channel_id: text("channel_id"),
        name: text("name"),
        description: text("description"),
        visibility: text("visibility"),
        video_url: text("video_url"),
        status_id: None,
        entry_id: text("entry_id"),
        position: small("position")?,
        start_seconds: small("start_seconds")?,
        stop_seconds: small("stop_seconds")?,
        revision: number("revision")?,
        confirm: text("confirm"),
    };
    Ok((pairs, input))
}
pub async fn create(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let result = async {
        let (pairs, input) = parse(&body)?;
        if !user.csrf_ok(field(&pairs, "csrf").unwrap_or("")) {
            return Ok(csrf_rejection());
        }
        match service::create(&state, &user.current.account, &input).await {
            Ok(p) => Ok(redirect_to(&format!("/videos/playlists/{}", p.id))),
            Err(ApiError::Unprocessable(message)) => {
                let mut response = render_index(
                    &state,
                    Some(&user),
                    user.locale,
                    &QueryParams::default(),
                    Some(&message),
                )
                .await?;
                *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }
    .await;
    private(result.unwrap_or_else(|e: ApiError| e.into_response()))
}
pub async fn change(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let result = async {
        let (pairs, input) = parse(&body)?;
        if !user.csrf_ok(field(&pairs, "csrf").unwrap_or("")) {
            return Ok(csrf_rejection());
        }
        match service::change(&state, &user.current.account, id, &input).await {
            Ok(p) => Ok(redirect_to(&if p.deleted_at.is_some() {
                "/videos/playlists?mine=true".to_owned()
            } else {
                format!("/videos/playlists/{id}")
            })),
            Err(ApiError::Unprocessable(message) | ApiError::Conflict(message)) => {
                let mut response = render(
                    &state,
                    Some(&user),
                    user.locale,
                    id,
                    &QueryParams::default(),
                    Some(&message),
                )
                .await?;
                *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }
    .await;
    private(result.unwrap_or_else(|e: ApiError| e.into_response()))
}
pub async fn import(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let result = async {
        let pairs = form_pairs(&body).map_err(ApiError::Unprocessable)?;
        if !user.csrf_ok(field(&pairs, "csrf").unwrap_or("")) {
            return Ok(csrf_rejection());
        }
        match crate::video_playlists_remote::import(
            &state,
            field(&pairs, "url").unwrap_or("").trim(),
            None,
        )
        .await
        {
            Ok(p) => Ok(redirect_to(&format!("/videos/playlists/{}", p.id))),
            Err(ApiError::Unprocessable(message)) => {
                let mut response = render_index(
                    &state,
                    Some(&user),
                    user.locale,
                    &QueryParams::default(),
                    Some(&message),
                )
                .await?;
                *response.status_mut() = StatusCode::UNPROCESSABLE_ENTITY;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }
    .await;
    private(result.unwrap_or_else(|e: ApiError| e.into_response()))
}
