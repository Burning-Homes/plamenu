//! HTML-first live creation and credential management. The key is never put
//! in a URL, cookie, log, GET response or database plaintext.

use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::{account, native_live};
use serde::Deserialize;

use super::{
    i18n::Locale,
    layout,
    session::{MaybeWebUser, WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};

fn private(result: Result<Response, ApiError>) -> Response {
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn playback_fields(pairs: &[(String, String)]) -> Result<native_live::PlaybackOptions, ApiError> {
    let minutes = field(pairs, "dvr_minutes")
        .unwrap_or("10")
        .parse::<i32>()
        .map_err(|_| ApiError::Unprocessable("Choose a DVR window".into()))?;
    let latency_mode = match field(pairs, "latency").unwrap_or("balanced") {
        "balanced" => 1,
        "resilient" => 2,
        "low" => 3,
        _ => return Err(ApiError::Unprocessable("Choose a latency mode".into())),
    };
    let options = native_live::PlaybackOptions {
        dvr_window_seconds: minutes
            .checked_mul(60)
            .ok_or_else(|| ApiError::Unprocessable("Choose a supported DVR window".into()))?,
        latency_mode,
    };
    if !options.valid() {
        return Err(ApiError::Unprocessable(
            "Choose a supported DVR window".into(),
        ));
    }
    Ok(options)
}

fn playback_controls(locale: Locale, selected: native_live::PlaybackOptions) -> Markup {
    html! {
        label.settings-field {
            span.settings-field__label { (locale.text("videos-live-dvr-window")) }
            select name="dvr_minutes" required {
                @for (minutes, key) in [(0, "videos-live-dvr-off"), (5, "videos-live-dvr-5"),
                    (10, "videos-live-dvr-10"), (15, "videos-live-dvr-15"),
                    (30, "videos-live-dvr-30")] {
                    option value=(minutes) selected[selected.dvr_window_seconds == minutes * 60] {
                        (locale.text(key))
                    }
                }
            }
        }
        label.settings-field {
            span.settings-field__label { (locale.text("videos-live-latency")) }
            select name="latency" required {
                option value="low" selected[selected.latency_mode == 3] {
                    (locale.text("videos-live-latency-low"))
                }
                option value="balanced" selected[selected.latency_mode == 1] {
                    (locale.text("videos-live-latency-balanced"))
                }
                option value="resilient" selected[selected.latency_mode == 2] {
                    (locale.text("videos-live-latency-resilient"))
                }
            }
        }
        p.settings-field__hint { (locale.text("videos-live-playback-hint")) }
    }
}

fn schedule_form(user: &WebUser, id: i64, starts: &[time::OffsetDateTime]) -> Markup {
    let locale = user.locale;
    let additional = starts
        .iter()
        .skip(1)
        .map(|at| user.clock.input_value(*at))
        .collect::<Vec<_>>()
        .join("\n");
    html! {
        form.settings-form method="post" action=(format!("/web/videos/live/{id}/schedule")) data-live-schedules
            data-added=(locale.text("videos-live-schedule-added"))
            data-duplicate=(locale.text("videos-live-schedule-duplicate")) {
            input type="hidden" name="csrf" value=(user.csrf);
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-planned-start")) }
                input type="datetime-local" name="scheduled_at"
                    value=(starts.first().map(|at| user.clock.input_value(*at)).unwrap_or_default());
            }
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-additional-starts")) }
                textarea name="scheduled_starts" rows="4" { (additional) }
            }
            p.settings-field__hint { (locale.text("videos-live-additional-starts-hint")) }
            (schedule_picker(locale))
            p.settings-field__hint {
                (locale.text("videos-live-schedule-hint")) " " (user.clock.name())
            }
            button type="submit" { (locale.text("videos-live-save-schedule")) }
        }
    }
}

async fn creation_form(state: &AppState, user: &WebUser) -> Result<Markup, ApiError> {
    let owned =
        plamenu_db::video_channel_access::available(&state.pool, user.current.account.id).await?;
    let ids = owned
        .iter()
        .map(|channel| channel.account_id)
        .collect::<Vec<_>>();
    let channels = account::find_by_ids(&state.pool, &ids).await?;
    let locale = user.locale;
    let body = html! {
        section.column {
            h1 { (locale.text("videos-live-title")) }
            p { (locale.text("videos-live-intro")) }
            @if channels.is_empty() {
                p { a href="/videos" { (locale.text("videos-new-channel")) } }
            } @else {
                form.settings-form method="post" action="/web/videos/live" data-live-schedules
                    data-added=(locale.text("videos-live-schedule-added"))
                    data-duplicate=(locale.text("videos-live-schedule-duplicate")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    fieldset.settings-form__group {
                        legend { (locale.text("videos-details")) }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-channel")) }
                            select name="channel" required {
                                @for channel in &channels {
                                    option value=(channel.id) { (channel.display_name) }
                                }
                            }
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-name")) }
                            input name="title" minlength="3" maxlength="120" required;
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-description")) }
                            textarea name="description" rows="5" maxlength="10000" {}
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-language")) }
                            input name="language" placeholder="en" maxlength="35";
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-visibility")) }
                            select name="visibility" {
                                option value="public" { (locale.text("videos-public")) }
                                option value="unlisted" { (locale.text("videos-unlisted")) }
                            }
                        }
                        label.settings-field__choice {
                            input type="checkbox" name="permanent" value="yes";
                            span.settings-field__label { (locale.text("videos-live-permanent")) }
                        }
                        (playback_controls(locale, native_live::PlaybackOptions {
                            dvr_window_seconds: 600,
                            latency_mode: 1,
                        }))
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-live-planned-start")) }
                            input type="datetime-local" name="scheduled_at";
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-live-additional-starts")) }
                            textarea name="scheduled_starts" rows="4" {}
                        }
                        p.settings-field__hint { (locale.text("videos-live-additional-starts-hint")) }
                        (schedule_picker(locale))
                        p.settings-field__hint {
                            (locale.text("videos-live-schedule-hint")) " " (user.clock.name())
                        }
                    }
                    button type="submit" { (locale.text("videos-live-create")) }
                }
            }
        }
    };
    Ok(layout::shell(
        &locale.text("videos-live-title"),
        Some(user),
        &body,
    ))
}

fn schedule_picker(locale: Locale) -> Markup {
    html! {
        div hidden data-live-schedule-picker {
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-pick-another-start")) }
                input type="datetime-local" data-live-schedule-date;
            }
            button type="button" data-live-schedule-add { (locale.text("videos-live-add-start")) }
            p role="status" aria-live="polite" data-live-schedule-status {}
        }
    }
}

pub async fn new_page(State(state): State<AppState>, user: WebUser) -> Response {
    private(
        creation_form(&state, &user)
            .await
            .map(IntoResponse::into_response),
    )
}

#[derive(Deserialize)]
pub struct RewindQuery {
    #[serde(default)]
    slice: i32,
}

/// Native controls and an ordinary GET form keep the DVR reachable with
/// scripts disabled; each selection gets a finite, seekable MP4 snapshot.
pub async fn rewind_page(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(query): Query<RewindQuery>,
    MaybeWebUser(session): MaybeWebUser,
    request_locale: Locale,
) -> Result<Response, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let live = native_live::public_dvr(&state.pool, media_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let media = plamenu_db::media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    let status_id = media.status_id.ok_or(ApiError::NotFound)?;
    let locale = session.as_ref().map_or(request_locale, |user| user.locale);
    let elapsed = (time::OffsetDateTime::now_utc() - live.started_at)
        .whole_seconds()
        .max(0);
    let configured = live.window_seconds / crate::routes::live_dvr::CLIP_SECONDS;
    let available =
        (elapsed / i64::from(crate::routes::live_dvr::CLIP_SECONDS) + 1).min(i64::from(configured));
    if query.slice < 0 || i64::from(query.slice) >= available {
        return Err(ApiError::NotFound);
    }
    let bucket = crate::routes::live_dvr::current_bucket();
    let src = crate::routes::live_dvr::clip_url(media_id, bucket, query.slice);
    let body = html! {
        section.column {
            h1 { (locale.text("status-live-rewind")) }
            p { (locale.text("status-live-rewind-intro")) }
            p { a href=(format!("/web/statuses/{status_id}")) { (locale.text("status-live-return")) } }
            form.settings-form method="get" action=(format!("/videos/live/{media_id}/rewind")) {
                label.settings-field {
                    span.settings-field__label { (locale.text("status-live-rewind-position")) }
                    select name="slice" {
                        @for slice in 0..available {
                            @let start = slice * 5;
                            @let end = start + 5;
                            option value=(slice) selected[i64::from(query.slice) == slice] {
                                (start) "–" (end)
                            }
                        }
                    }
                }
                button type="submit" { (locale.text("status-live-rewind-load")) }
            }
            video.live-rewind-player src=(src) controls playsinline preload="metadata"
                aria-label=(locale.text("status-live-rewind-player")) {}
            p.settings-field__hint { (locale.text("status-live-rewind-hint")) }
        }
    };
    Ok(layout::shell(&locale.text("status-live-rewind"), session.as_ref(), &body).into_response())
}

fn key_page(state: &AppState, user: &WebUser, status_id: i64, title: &str, key: &str) -> Markup {
    let locale = user.locale;
    let bind = state.config.live_ingest_bind;
    let host = if bind.ip().is_loopback() {
        "127.0.0.1"
    } else {
        &state.config.domain
    };
    let server = format!("rtmp://{host}:{}/live", bind.port());
    let secure_server = state.config.live_rtmps.as_ref().map(|rtmps| {
        let host = if rtmps.bind.ip().is_loopback() {
            "127.0.0.1"
        } else {
            &state.config.domain
        };
        format!("rtmps://{host}:{}/live", rtmps.bind.port())
    });
    let https_ingest = format!("https://{}/live/ingest", state.config.domain);
    let body = html! {
        section.column {
            h1 { (locale.text("videos-live-manage")) }
            p { (title) }
            p role="status" { (locale.text("videos-live-waiting")) }
            p { (locale.text("videos-live-secret-once")) }
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-server")) }
                input type="text" readonly value=(server);
            }
            @if let Some(secure_server) = secure_server {
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-live-secure-server")) }
                    input type="text" readonly value=(secure_server);
                }
                p.settings-field__hint { (locale.text("videos-live-rtmps-help")) }
            }
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-secret")) }
                input type="text" readonly value=(key);
            }
            label.settings-field {
                span.settings-field__label { (locale.text("videos-live-https-server")) }
                input type="text" readonly value=(https_ingest);
            }
            p.settings-field__hint { (locale.text("videos-live-https-help")) }
            p.settings-field__hint { (locale.text("videos-live-transport-warning")) }
            p { a href=(format!("/videos/live/{status_id}")) { (locale.text("videos-live-manage")) } }
            p { a href=(format!("/web/statuses/{status_id}")) { (title) } }
        }
    };
    layout::shell(&locale.text("videos-live-manage"), Some(user), &body)
}

pub async fn create(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    private(create_inner(&state, &user, &body).await)
}

async fn create_inner(
    state: &AppState,
    user: &WebUser,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let channel_id = field(&pairs, "channel")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(ApiError::Unprocessable("Choose a video channel".into()))?;
    let title = field(&pairs, "title").unwrap_or_default().trim();
    let description = field(&pairs, "description").unwrap_or_default().trim();
    let language = field(&pairs, "language").filter(|value| !value.is_empty());
    let visibility = field(&pairs, "visibility").unwrap_or("public");
    let permanent = field(&pairs, "permanent") == Some("yes");
    let playback = playback_fields(&pairs)?;
    let scheduled_starts = schedule_fields(state, user, &pairs).await?;
    let created = crate::native_live::create(
        state,
        &user.current.account,
        channel_id,
        title,
        description,
        language,
        visibility,
        permanent,
        playback,
        &scheduled_starts,
    )
    .await?;
    Ok(key_page(state, user, created.status.id, title, &created.ingest_key).into_response())
}

async fn schedule_fields(
    state: &AppState,
    user: &WebUser,
    pairs: &[(String, String)],
) -> Result<Vec<time::OffsetDateTime>, ApiError> {
    let first = field(pairs, "scheduled_at").unwrap_or_default().trim();
    let additional = field(pairs, "scheduled_starts").unwrap_or_default();
    let raw = std::iter::once(first)
        .chain(additional.lines().map(str::trim))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if raw.len() > 16 {
        return Err(ApiError::Unprocessable(
            "Choose no more than 16 planned starts".into(),
        ));
    }
    let mut starts = Vec::with_capacity(raw.len());
    for value in raw {
        starts.push(
            super::scheduled::resolve_local_instant(state, user, value, "Planned start").await?,
        );
    }
    starts.sort_unstable();
    crate::native_live::validate_schedules(&starts)?;
    Ok(starts)
}

pub async fn manage(State(state): State<AppState>, Path(id): Path<i64>, user: WebUser) -> Response {
    private(manage_inner(&state, &user, id).await)
}

pub async fn update_playback(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(update_playback_inner(&state, &user, id, &body).await)
}

pub async fn update_schedule(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(update_schedule_inner(&state, &user, id, &body).await)
}

async fn update_schedule_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let planned = schedule_fields(state, user, &pairs).await?;
    if !native_live::update_schedule(&state.pool, id, user.current.account.id, &planned).await? {
        return Err(ApiError::Conflict(
            "The planned start can only change while the broadcast is waiting".into(),
        ));
    }
    if let Err(error) = crate::native_live::federate_state(state, id).await {
        tracing::warn!(%error, status_id = id, "could not announce live schedule");
    }
    Ok(redirect_to(&format!("/videos/live/{id}")))
}

async fn update_playback_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let playback = playback_fields(&pairs)?;
    if !native_live::update_playback(&state.pool, id, user.current.account.id, playback).await? {
        return Err(ApiError::Conflict(
            "Playback settings can only change while the broadcast is waiting".into(),
        ));
    }
    if let Err(error) = crate::native_live::federate_state(state, id).await {
        tracing::warn!(%error, status_id = id, "could not announce live playback settings");
    }
    Ok(redirect_to(&format!("/videos/live/{id}")))
}

async fn manage_inner(state: &AppState, user: &WebUser, id: i64) -> Result<Response, ApiError> {
    let broadcast = native_live::owned_by_status(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let replay_ready = broadcast.state == "ended"
        && plamenu_db::media::find_by_ids(&state.pool, &[broadcast.media_id])
            .await?
            .first()
            .is_some_and(|media| media.file_name.is_some());
    let sessions = if broadcast.permanent {
        native_live::session_replays(&state.pool, broadcast.media_id).await?
    } else {
        Vec::new()
    };
    let locale = user.locale;
    let state_label = live_state_label(&broadcast.state);
    let calendar_url = format!(
        "https://{}/videos/live/{id}/calendar.ics",
        state.config.domain
    );
    let body = html! {
        section.column {
            h1 { (locale.text("videos-live-manage")) }
            h2 { (status.title.unwrap_or_default()) }
            p role="status" { (locale.text(state_label)) }
            @if replay_ready {
                p { a href=(format!("/web/statuses/{id}")) { (locale.text("videos-live-watch-replay")) } }
            } @else if broadcast.state == "ended" && !broadcast.permanent {
                p { (locale.text("videos-live-replay-processing")) }
            }
            @if broadcast.permanent && !sessions.is_empty() {
                section aria-labelledby="live-session-replays" {
                    h3 id="live-session-replays" { (locale.text("videos-live-session-replays")) }
                    ol {
                        @for session in &sessions {
                            @if let Some(replay_status_id) = session.status_id {
                                li { a href=(format!("/web/statuses/{replay_status_id}")) {
                                    (locale.text("videos-live-watch-replay")) " " (session.generation)
                                } }
                            } @else if session.ended_at.is_some() {
                                li { (locale.text("videos-live-replay-processing")) " " (session.generation) }
                            }
                        }
                    }
                }
            }
            @if broadcast.state == "interrupted" {
                p { (locale.text("videos-live-reconnect-help")) }
            }
            p { (locale.text("videos-live-key-hidden")) }
            @if broadcast.state == "waiting" {
                (schedule_form(user, id, &broadcast.scheduled_starts))
                section aria-labelledby="live-calendar-heading" {
                    h3 id="live-calendar-heading" { (locale.text("videos-live-calendar-heading")) }
                    p { (locale.text("videos-live-calendar-hint")) }
                    label for="live-calendar-url" { (locale.text("videos-live-calendar-broadcast")) }
                    input id="live-calendar-url" type="url" readonly value=(&calendar_url);
                    p { a href=(&calendar_url) {
                        (locale.text("videos-live-calendar-broadcast"))
                    } }
                    p { a href=(format!("{calendar_url}?reminder=15")) {
                        (locale.text("videos-live-calendar-reminder"))
                    } }
                }
                form.settings-form method="post" action=(format!("/web/videos/live/{id}/playback")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    fieldset.settings-form__group {
                        legend { (locale.text("videos-live-playback-settings")) }
                        (playback_controls(locale, native_live::PlaybackOptions {
                            dvr_window_seconds: broadcast.dvr_window_seconds,
                            latency_mode: broadcast.latency_mode,
                        }))
                    }
                    button type="submit" { (locale.text("videos-live-save-playback")) }
                }
            }
            form.settings-form method="post" action=(format!("/web/videos/live/{id}/rotate-key")) {
                input type="hidden" name="csrf" value=(user.csrf);
                p.settings-field__hint id="live-rotate-warning" { (locale.text("videos-live-rotate-warning")) }
                button type="submit" aria-describedby="live-rotate-warning" { (locale.text("videos-live-rotate")) }
            }
            @if broadcast.state != "ended" {
                (browser_publisher(locale, user, id))
            }
            p { a href=(format!("/web/statuses/{id}")) { (locale.text("videos-preview")) } }
        }
    };
    Ok(layout::shell(&locale.text("videos-live-manage"), Some(user), &body).into_response())
}

fn browser_publisher(locale: Locale, user: &WebUser, id: i64) -> Markup {
    html! {
        section.live-browser-publisher hidden data-live-browser
            data-status-id=(id) data-csrf=(user.csrf)
            data-unsupported=(locale.text("videos-live-browser-unsupported"))
            data-connection-error=(locale.text("videos-live-browser-connection-error"))
            data-start-error=(locale.text("videos-live-browser-start-error"))
            data-start-timeout=(locale.text("videos-live-browser-start-timeout"))
            data-permission-denied=(locale.text("videos-live-browser-permission-denied"))
            data-device-unavailable=(locale.text("videos-live-browser-device-unavailable"))
            data-device-busy=(locale.text("videos-live-browser-device-busy"))
            data-capture-cancelled=(locale.text("videos-live-browser-capture-cancelled"))
            data-requesting=(locale.text("videos-live-browser-requesting"))
            data-slow-connection=(locale.text("videos-live-browser-slow"))
            data-on-air=(locale.text("videos-live-browser-on-air"))
            data-current-camera=(locale.text("videos-live-browser-current-camera"))
            data-current-screen=(locale.text("videos-live-browser-current-screen"))
            data-switching=(locale.text("videos-live-browser-switching"))
            data-switch-failed=(locale.text("videos-live-browser-switch-failed"))
            data-switch-camera=(locale.text("videos-live-browser-switch-camera"))
            data-switch-screen=(locale.text("videos-live-browser-switch-screen"))
            data-live-camera-label=(locale.text("videos-live-browser-camera-live"))
            data-live-screen-label=(locale.text("videos-live-browser-screen-live"))
            data-stopped=(locale.text("videos-live-browser-stopped"))
            data-camera-unknown=(locale.text("videos-live-browser-unknown-camera"))
            data-mic-unknown=(locale.text("videos-live-browser-unknown-mic")) {
            h2 { (locale.text("videos-live-browser-title")) }
            p { (locale.text("videos-live-browser-intro")) }
            (browser_capture_controls(locale))
            div.live-browser-publisher__actions {
                button type="button" data-live-camera { (locale.text("videos-live-browser-camera")) }
                button type="button" data-live-screen { (locale.text("videos-live-browser-screen")) }
                button type="button" data-live-stop hidden { (locale.text("videos-live-browser-stop")) }
            }
            p role="status" aria-live="polite" data-live-status {}
            video autoplay muted playsinline hidden data-live-preview
                aria-label=(locale.text("videos-live-browser-preview")) {}
        }
        script defer src="/assets/live-broadcast.js" {}
    }
}

fn browser_capture_controls(locale: Locale) -> Markup {
    html! {
        fieldset.live-browser-publisher__devices hidden data-live-devices {
            legend { (locale.text("videos-live-browser-devices")) }
            label {
                span { (locale.text("videos-live-browser-camera-device")) }
                select data-live-camera-device {
                    option value="" { (locale.text("videos-live-browser-default-camera")) }
                }
            }
            label {
                span { (locale.text("videos-live-browser-mic-device")) }
                select data-live-mic-device {
                    option value="" { (locale.text("videos-live-browser-default-mic")) }
                }
            }
            label.live-browser-publisher__screen-audio {
                input type="checkbox" checked data-live-screen-audio;
                (locale.text("videos-live-browser-screen-audio"))
            }
            p.settings-field__hint { (locale.text("videos-live-browser-device-hint")) }
        }
    }
}

fn live_state_label(state: &str) -> &'static str {
    match state {
        "live" => "videos-live-state-live",
        "interrupted" => "videos-live-state-interrupted",
        "ended" => "videos-live-state-ended",
        _ => "videos-live-state-waiting",
    }
}

pub async fn rotate_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(rotate_inner(&state, &user, id, &body).await)
}

#[derive(Deserialize)]
struct BrowserHello {
    csrf: String,
    mime: String,
}

/// The browser submits encoded camera/screen chunks over its same-origin
/// authenticated socket. The first message supplies CSRF and container type;
/// neither the session cookie nor the RTMP key is exposed to page JavaScript.
pub async fn browser_publish(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        != Some(format!("https://{}", state.config.domain).as_str())
    {
        return Err(ApiError::Forbidden("cross-origin broadcast refused".into()));
    }
    let broadcast = native_live::owned_by_status(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if broadcast.state == "ended" {
        return Err(ApiError::Gone);
    }
    Ok(upgrade
        .max_message_size(2 * 1024 * 1024)
        .max_frame_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| browser_socket(state, id, user, socket))
        .into_response())
}

async fn browser_socket(state: AppState, status_id: i64, user: WebUser, mut socket: WebSocket) {
    let hello = tokio::time::timeout(std::time::Duration::from_secs(10), socket.recv()).await;
    let Some(Ok(Message::Text(text))) = hello.ok().flatten() else {
        return;
    };
    let Ok(hello) = serde_json::from_str::<BrowserHello>(&text) else {
        return;
    };
    if !user.csrf_ok(&hello.csrf) {
        return;
    }
    let format = if hello.mime.starts_with("video/webm") {
        crate::live_ingest::InputFormat::BrowserWebm
    } else if hello.mime.starts_with("video/mp4") {
        crate::live_ingest::InputFormat::BrowserMp4
    } else {
        let _ = socket
            .send(Message::Text(
                "{\"error\":\"Unsupported recording format\"}".into(),
            ))
            .await;
        return;
    };
    let Ok(Some(session)) =
        native_live::claim_owned(&state.pool, status_id, user.current.account.id).await
    else {
        let _ = socket
            .send(Message::Text(
                "{\"error\":\"Broadcast already in use or ended\"}".into(),
            ))
            .await;
        return;
    };
    let sender = match crate::live_ingest::start_piped(&state, session.clone(), format).await {
        Ok(sender) => sender,
        Err(error) => {
            tracing::warn!(%error, media = session.media_id, "cannot prepare browser live ingest");
            let _ = native_live::finish(
                &state.pool,
                &session,
                native_live::FinishReason::Interrupted,
            )
            .await;
            return;
        }
    };
    if let Err(error) = crate::native_live::federate_state(&state, session.status_id).await {
        tracing::warn!(%error, media = session.media_id, "could not announce browser live start");
    }
    if socket
        .send(Message::Text("{\"ready\":true}".into()))
        .await
        .is_err()
    {
        return;
    }
    while let Ok(Some(message)) =
        tokio::time::timeout(std::time::Duration::from_secs(15), socket.recv()).await
    {
        match message {
            Ok(Message::Binary(bytes)) if !bytes.is_empty() => {
                if sender.sender.send(bytes.to_vec()).await.is_err() {
                    break;
                }
            }
            Ok(Message::Text(text)) if text == "stop" => {
                sender.mark_stopped();
                let _ = socket
                    .send(Message::Text("{\"stopped\":true}".into()))
                    .await;
                break;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

async fn rotate_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let key = crate::native_live::rotate_ingest_key(state, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let status = plamenu_db::status::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(key_page(
        state,
        user,
        id,
        status.title.as_deref().unwrap_or_default(),
        &key,
    )
    .into_response())
}
