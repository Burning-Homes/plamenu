//! HTML-first native video publishing. Upload/review and publication are
//! separate submissions: a failed metadata validation never discards the file.
use super::{
    i18n::Locale,
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, actions::PostParams, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::{account, id, media, video_draft};
use tokio::io::AsyncWriteExt;

#[derive(Default)]
pub(super) struct Draft {
    pub channel: String,
    pub title: String,
    pub description: String,
    pub language: String,
    pub visibility: String,
    pub alt: String,
    pub transcript: String,
    pub poster_time: String,
    pub poster_alt: Option<String>,
    pub media_id: Option<i64>,
    pub scheduled_at: String,
    pub category: String,
    pub licence: String,
    pub spoiler_text: String,
    pub sensitive: bool,
}

async fn owned_preview(
    state: &AppState,
    owner: i64,
    id: Option<i64>,
) -> Result<Option<media::Media>, ApiError> {
    let Some(id) = id else { return Ok(None) };
    Ok(media::find_by_ids(&state.pool, &[id])
        .await?
        .into_iter()
        .find(|m| m.account_id == owner && m.status_id.is_none()))
}

fn poster_picker(draft: &Draft, locale: Locale) -> Markup {
    html! {
        label.settings-field {
            span.settings-field__label { (locale.text("videos-poster-time")) }
            input type="number" name="poster_time" min="0" max="86400" step="0.1" aria-describedby="video-poster-time-hint" value=(if draft.poster_time.is_empty() { "0" } else { &draft.poster_time }) data-poster-time;
        }
        p.settings-field__hint id="video-poster-time-hint" { (locale.text("videos-poster-time-hint")) }
        div.video-poster-picker data-poster-picker data-poster-status-prefix=(locale.text("videos-poster-at")) hidden {
            video controls preload="metadata" muted playsinline aria-label=(locale.text("videos-poster-picker")) data-poster-source {}
            button type="button" data-poster-use-current { (locale.text("videos-poster-use-current")) }
            p role="status" aria-live="polite" data-poster-picker-status {}
        }
    }
}

fn review_poster(media_id: i64, time: &str, alt: Option<&str>, locale: Locale) -> Markup {
    let default_alt = locale.text("videos-poster-preview");
    html! {
        figure.video-publish__poster {
            img src=(format!("/videos/drafts/{media_id}/preview/poster")) alt=(alt.unwrap_or(&default_alt));
            figcaption {
                @if alt.is_some() { (locale.text("videos-custom-poster")) }
                @else { (locale.text("videos-poster-at")) " " (time) " s" }
                " · " a href="#video-poster-replace" { (locale.text("videos-replace-poster")) }
            }
        }
    }
}

fn replace_poster_form(media_id: i64, csrf: &str, locale: Locale) -> Markup {
    html! {
        section id="video-poster-replace" {
            h2 { (locale.text("videos-replace-poster")) }
            p.settings-field__hint { (locale.text("videos-replace-poster-hint")) }
            form.settings-form data-video-poster-replace method="post"
                action=(format!("/web/videos/drafts/{media_id}/poster")) enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(csrf);
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-poster-alt")) }
                    textarea name="poster_alt" rows="3" maxlength="500" required {}
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-poster-file")) }
                    input type="file" name="poster_file" accept="image/png,image/jpeg,image/webp,image/avif,image/jxl,image/gif" required;
                }
                img.video-poster-replace__preview data-poster-replace-preview
                    alt=(locale.text("videos-new-poster-preview")) hidden;
                button type="submit" { (locale.text("videos-save-poster")) }
            }
        }
    }
}

fn new_channel_form(csrf: &str, expand: bool, locale: Locale) -> Markup {
    html! {
        details open[expand] {
            summary { (locale.text("videos-new-channel")) }
            form.settings-form method="post" action="/web/video-channels" {
                input type="hidden" name="csrf" value=(csrf);
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-channel-address")) }
                    input name="username" pattern="[a-zA-Z0-9_]+" maxlength="30" required;
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-channel-name")) }
                    input name="display_name" maxlength="120" required;
                }
                button type="submit" { (locale.text("videos-new-channel")) }
            }
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one HTML authoring/review template")]
pub(super) async fn page(
    state: &AppState,
    user: &WebUser,
    draft: &Draft,
    error: Option<&str>,
) -> Result<Markup, ApiError> {
    let owned =
        plamenu_db::video_channel_access::available(&state.pool, user.current.account.id).await?;
    let ids = owned.iter().map(|c| c.account_id).collect::<Vec<_>>();
    let channels = account::find_by_ids(&state.pool, &ids).await?;
    let locale = user.locale;
    let preview = owned_preview(state, user.current.account.id, draft.media_id).await?;
    let recent = if preview.is_none() {
        plamenu_db::video_draft::recent_owned(&state.pool, user.current.account.id).await?
    } else {
        Vec::new()
    };
    let recent_published = if preview.is_none() {
        plamenu_db::video_captions::recent_owned_publications(&state.pool, user.current.account.id)
            .await?
    } else {
        Vec::new()
    };
    let live_broadcasts =
        plamenu_db::native_live::recent_owned(&state.pool, user.current.account.id).await?;
    // Derive privacy from stored ownership, never a client-submitted flag.
    let native = if let Some(media) = &preview {
        plamenu_db::video_draft::find_owned(&state.pool, media.id, user.current.account.id)
            .await?
            .is_some()
    } else {
        false
    };
    let caption_tracks = if let Some(media) = &preview {
        plamenu_db::video_captions::for_media(&state.pool, &[media.id])
            .await?
            .remove(&media.id)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let chapter_tracks = if let Some(media) = &preview {
        plamenu_db::video_chapters::for_media(&state.pool, &[media.id])
            .await?
            .remove(&media.id)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let body = html! {
        section.column {
            h1 { (locale.text("videos-title")) }
            @if let Some(message) = error { p.settings__error role="alert" { (message) } }
            p { (locale.text("videos-intro")) }
            p { a href="/videos/explore" { (locale.text("videos-explore-browse")) } }
            p { a href="/videos/channels" { (locale.text("video-channel-manage")) } }
            @if !recent.is_empty() {
                nav aria-label=(locale.text("videos-recent-drafts")) {
                    h2 { (locale.text("videos-recent-drafts")) }
                    ul {
                        @for pending in &recent {
                            li { a href=(format!("/videos/drafts/{}", pending.media_id)) { (pending.title) } }
                        }
                    }
                }
            }
            @if !recent_published.is_empty() {
                nav aria-label=(locale.text("videos-your-published")) {
                    h2 { (locale.text("videos-your-published")) }
                    ul {
                        @for published in &recent_published {
                            li {
                                a href=(format!("/videos/{}/captions", published.media_id)) {
                                    (published.title) " — " (locale.text("videos-caption-published-title"))
                                }
                                " · "
                                a href=(format!("/videos/{}/chapters", published.media_id)) {
                                    (locale.text("videos-chapters-title"))
                                }
                                " · " a href=(format!("/videos/{}/metadata", published.media_id)) { (locale.text("videos-metadata-manage")) }
                                " · " a href=(format!("/web/statuses/{}/edit",published.status_id)) { (locale.text("status-edit")) }
                            }
                        }
                    }
                }
            }
            @if !live_broadcasts.is_empty() {
                nav aria-label=(locale.text("videos-live-your-broadcasts")) {
                    h2 { (locale.text("videos-live-your-broadcasts")) }
                    ul {
                        @for broadcast in &live_broadcasts {
                            @let state_label = match broadcast.state.as_str() {
                                "live" => "videos-live-state-live",
                                "interrupted" => "videos-live-state-interrupted",
                                "ended" => "videos-live-state-ended",
                                _ => "videos-live-state-waiting",
                            };
                            li {
                                a href=(format!("/videos/live/{}", broadcast.status_id)) {
                                    (broadcast.title) " — " (locale.text(state_label))
                                }
                                @if let Some(at) = broadcast.scheduled_at {
                                    " — " (locale.text("status-live-planned-start")) " "
                                    (user.clock.element_absolute(at))
                                }
                            }
                        }
                    }
                }
            }
            @if !channels.is_empty() {
                p { a href="/videos/live/new" { (locale.text("videos-live-start")) } }
                @if preview.is_none() {
                nav aria-label=(locale.text("videos-your-channels")) {
                    h2 { (locale.text("videos-your-channels")) }
                    ul {
                        @for channel in &channels {
                            @let name = if channel.display_name.is_empty() {
                                &channel.username
                            } else {
                                &channel.display_name
                            };
                            li { a href=(format!("/videos/explore?channel={}", channel.id)) { (name) }
                                " · " a href=(format!("/videos/channels/{}",channel.id)) { (locale.text("video-channel-manage")) }
                            }
                        }
                    }
                }
                }
                form.settings-form data-video-upload[preview.is_none()] data-video-schedule-form[preview.is_some()] method="post" action="/web/videos" enctype="multipart/form-data" {
                    input type="hidden" name="csrf" value=(user.csrf);
                    @if let Some(media) = &preview {
                        input type="hidden" name="media_id" value=(media.id);
                        h2 { (locale.text("videos-review")) }
                        @if let Some(file) = &media.file_name {
                            video.video-publish__preview controls preload="metadata" aria-label=(locale.text("videos-preview")) {
                                source src=(if native { format!("/videos/drafts/{}/preview/video", media.id) } else { format!("/media/{file}") }) type=(media.content_type);
                                @for (index, track) in caption_tracks.iter().enumerate() {
                                    @let src = if track.legacy {
                                        format!("/videos/drafts/{}/preview/captions.vtt", media.id)
                                    } else {
                                        format!("/videos/drafts/{}/captions/{}.vtt", media.id, track.language)
                                    };
                                    track kind="captions" src=(src) srclang=(track.language)
                                        label=(track.language) default[index == 0];
                                }
                            }
                        }
                        @if native {
                            (review_poster(media.id, &draft.poster_time, draft.poster_alt.as_deref(), locale))
                        }
                        @if !draft.alt.is_empty() { details { summary { (locale.text("videos-alt")) } p { (draft.alt) } } }
                        @if !draft.transcript.is_empty() { details { summary { (locale.text("videos-transcript")) } p { (draft.transcript) } } }
                    }
                    fieldset.settings-form__group {
                        legend { (locale.text("videos-details")) }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-channel")) }
                            @if native { input type="hidden" name="channel" value=(draft.channel); }
                            select name="channel" required disabled[native] {
                                @for channel in &channels {
                                    option value=(channel.id) selected[draft.channel == channel.id.to_string()] { (channel.display_name) }
                                }
                            }
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-name")) }
                            input name="title" value=(draft.title) minlength="3" maxlength="120" required;
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-description")) }
                            textarea name="description" rows="5" maxlength="10000" { (draft.description) }
                        }
                        p.settings-field__hint { (locale.text("videos-tags-hint")) }
                        (super::video_metadata::fields(&draft.category, &draft.licence, locale))
                        label.settings-field {
                            span.settings-field__label { (locale.text("compose-content-warning")) }
                            input name="spoiler_text" value=(draft.spoiler_text) maxlength="500";
                        }
                        label.settings-field {
                            input type="hidden" name="sensitive" value="false";
                            span.settings-field__label {
                                input type="checkbox" name="sensitive" value="true" checked[draft.sensitive];
                                " " (locale.text("compose-sensitive"))
                            }
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-language")) }
                            input name="language" value=(draft.language) placeholder="en" maxlength="35";
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-visibility")) }
                            select name="visibility" {
                                option value="public" selected[draft.visibility != "unlisted"] { (locale.text("videos-public")) }
                                option value="unlisted" selected[draft.visibility == "unlisted"] { (locale.text("videos-unlisted")) }
                            }
                        }
                    }
                    @if preview.is_none() {
                        fieldset.settings-form__group {
                            legend { (locale.text("videos-file")) }
                            p.settings-field__hint { (locale.text("videos-upload-hint")) " " (state.config.native_video.max_upload_mib) " MiB." }
                            (poster_picker(draft, locale))
                            label.settings-field {
                                span.settings-field__label { (locale.text("videos-file")) }
                                input type="file" name="file" accept="video/*,audio/*" required;
                            }
                            label.settings-field {
                                span.settings-field__label { (locale.text("videos-alt")) }
                                textarea name="alt" rows="3" { (draft.alt) }
                            }
                            label.settings-field {
                                span.settings-field__label { (locale.text("videos-captions")) }
                                input type="file" name="captions" accept=".vtt,text/vtt";
                            }
                            label.settings-field {
                                span.settings-field__label { (locale.text("videos-transcript")) }
                                textarea name="transcript" rows="5" { (draft.transcript) }
                            }
                        }
                    } @else {
                        input type="hidden" name="alt" value=(draft.alt);
                        input type="hidden" name="transcript" value=(draft.transcript);
                        fieldset.settings-form__group {
                            legend { (locale.text("videos-schedule-title")) }
                            label.settings-field {
                                span.settings-field__label { (locale.text("videos-schedule-at")) }
                                input type="datetime-local" name="scheduled_at" value=(draft.scheduled_at)
                                    aria-describedby="videos-schedule-hint" data-video-schedule-at;
                            }
                            p.settings-field__hint id="videos-schedule-hint" {
                                (locale.text("videos-schedule-hint")) " " (user.clock.label()) "."
                            }
                        }
                    }
                    @if preview.is_none() {
                        div.video-upload__progress data-video-upload-progress
                            data-uploading=(locale.text("videos-uploading"))
                            data-saving=(locale.text("videos-upload-saving"))
                            data-failed=(locale.text("videos-upload-failed"))
                            data-mismatch=(locale.text("videos-draft-file-mismatch"))
                            data-resume-link=(locale.text("videos-draft-open-to-resume"))
                            hidden {
                            p role="status" aria-live="polite" data-video-upload-label { (locale.text("videos-uploading")) " 0%" }
                            progress max="100" value="0" aria-label=(locale.text("videos-upload-progress")) data-video-upload-meter;
                        }
                        p.settings__error role="alert" tabindex="-1" data-video-upload-error hidden {}
                    }
                    div.settings-form__actions {
                        @if preview.is_some() {
                            button type="submit" name="op" value="publish" { (locale.text("videos-publish")) }
                            button type="submit" name="op" value="schedule" data-video-schedule-submit { (locale.text("videos-schedule-button")) }
                            a href="/videos" { (locale.text("videos-start-over")) }
                        } @else {
                            button type="submit" name="op" value="review" { (locale.text("videos-upload-review")) }
                        }
                    }
                }
                @if native {
                    @if let Some(media) = &preview {
                        section.video-captions id="caption-languages" {
                            h2 { (locale.text("videos-caption-manage")) }
                            p.settings-field__hint { (locale.text("videos-caption-manage-hint")) }
                            @if !caption_tracks.is_empty() {
                                ul {
                                    @for track in &caption_tracks {
                                        li {
                                            (track.language) " — "
                                            a href=(if track.legacy {
                                                format!("/videos/drafts/{}/preview/captions.vtt", media.id)
                                            } else {
                                                format!("/videos/drafts/{}/captions/{}.vtt", media.id, track.language)
                                            }) { (locale.text("videos-caption-open")) }
                                            form method="post" action=(format!("/web/videos/drafts/{}/captions/{}/remove", media.id, track.language)) {
                                                input type="hidden" name="csrf" value=(user.csrf);
                                                button.video-captions__remove type="submit"
                                                    aria-label=(format!("{} {}", locale.text("videos-caption-remove"), track.language)) {
                                                    (locale.text("videos-caption-remove"))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            form.settings-form data-caption-upload
                                data-caption-ready=(locale.text("videos-caption-ready"))
                                data-caption-invalid=(locale.text("videos-caption-invalid"))
                                data-caption-too-large=(locale.text("videos-caption-too-large"))
                                method="post" action=(format!("/web/videos/drafts/{}/captions", media.id)) enctype="multipart/form-data" {
                                input type="hidden" name="csrf" value=(user.csrf);
                                label.settings-field {
                                    span.settings-field__label { (locale.text("videos-caption-language")) }
                                    input name="language" required maxlength="35" pattern="[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*" placeholder="en";
                                }
                                label.settings-field {
                                    span.settings-field__label { (locale.text("videos-captions")) }
                                    input type="file" name="caption" accept=".vtt,text/vtt" required;
                                }
                                p role="status" aria-live="polite" data-caption-status hidden {}
                                button type="submit" { (locale.text("videos-caption-save")) }
                            }
                        }
                        (super::video_chapters::editor(media.id, &chapter_tracks, &user.csrf, false, locale))
                        (replace_poster_form(media.id, &user.csrf, locale))
                    }
                }
            }
            (new_channel_form(&user.csrf, channels.is_empty(), locale))
        }
    };
    Ok(layout::shell(
        &locale.text("videos-title"),
        Some(user),
        &body,
    ))
}

pub async fn index(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        page(&state, &user, &Draft::default(), None).await?,
    )
        .into_response())
}

/// Same-origin native controls work with cookies and without JavaScript. Every
/// response, including refusals and ranges, is private/non-cacheable. Resolve
/// only authentication here, not navigation badges/preferences for each seek.
pub async fn preview(
    State(state): State<AppState>,
    Path((id, kind)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    let mut response = match preview_inner(&state, id, &kind, &headers).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response
}

pub async fn draft_caption(
    State(state): State<AppState>,
    Path((id, name)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    let kind = format!("captions/{name}");
    let mut response = match preview_inner(&state, id, &kind, &headers).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response
}

async fn preview_inner(
    state: &AppState,
    id: i64,
    kind: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let caption_language = kind
        .strip_prefix("captions/")
        .and_then(|name| name.strip_suffix(".vtt"));
    if !matches!(kind, "video" | "poster" | "captions.vtt") && caption_language.is_none() {
        return Err(ApiError::NotFound);
    }
    let token = super::session::raw_session_token(headers).ok_or(ApiError::NotFound)?;
    let user = crate::auth::user_for_token(state, token)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let preview = plamenu_db::video_draft::preview(&state.pool, id, user.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if kind == "captions.vtt" {
        return Ok((
            [(header::CONTENT_TYPE, "text/vtt; charset=utf-8")],
            preview.caption_vtt.ok_or(ApiError::NotFound)?,
        )
            .into_response());
    }
    if let Some(language) = caption_language {
        let body =
            plamenu_db::video_captions::draft_body(&state.pool, id, language, user.account.id)
                .await?
                .ok_or(ApiError::NotFound)?;
        return Ok(([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], body).into_response());
    }
    let file = if kind == "video" {
        &preview.file_name
    } else {
        &preview.small_file_name
    };
    crate::routes::media::stream_stored_file(
        state,
        file,
        crate::media_processing::content_type_for(file),
        "private, no-store",
        headers,
    )
    .await
}

pub async fn create_channel(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    let Ok(pairs) = form_pairs(&body) else {
        return ApiError::BadRequest("Invalid form".into()).into_response();
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return csrf_rejection();
    }
    match crate::video::create_channel(
        &state,
        &user.current.account,
        field(&pairs, "username").unwrap_or_default(),
        field(&pairs, "display_name").unwrap_or_default(),
    )
    .await
    {
        Ok(_) => redirect_to("/videos"),
        Err(error) => refusal(&state, &user, &Draft::default(), error).await,
    }
}

async fn refusal(state: &AppState, user: &WebUser, draft: &Draft, error: ApiError) -> Response {
    match error {
        ApiError::Unprocessable(message) | ApiError::Forbidden(message) => {
            match page(state, user, draft, Some(&message)).await {
                Ok(page) => (axum::http::StatusCode::UNPROCESSABLE_ENTITY, page).into_response(),
                Err(error) => error.into_response(),
            }
        }
        other => other.into_response(),
    }
}

pub async fn submit(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    let mut draft = Draft::default();
    let mut reserved = None;
    let result = submit_inner(
        &state,
        &user,
        &headers,
        multipart,
        &mut draft,
        &mut reserved,
    )
    .await;
    if result.is_err()
        && let Some(id) = reserved
        && let Err(error) = video_draft::cancel(&state.pool, id, user.current.account.id).await
    {
        tracing::warn!(%error, media_id = id, "cannot cancel rejected video upload");
    }
    match result {
        Ok(response) => response,
        Err(error) => refusal(&state, &user, &draft, error).await,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded streaming multipart dispatch"
)]
async fn submit_inner(
    state: &AppState,
    user: &WebUser,
    headers: &HeaderMap,
    mut multipart: Multipart,
    draft: &mut Draft,
    reserved: &mut Option<i64>,
) -> Result<Response, ApiError> {
    let mut csrf = String::new();
    let mut operation = String::new();
    let mut upload = None;
    let mut captions = None;
    let file_limit =
        usize::try_from(state.config.native_video.max_upload_bytes()).unwrap_or(usize::MAX);
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return Err(ApiError::BadRequest("Invalid or oversized upload".into())),
        };
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            // The HTML form sends CSRF and publish metadata before the file.
            // Reserve both durable quota and free space before consuming bytes.
            if !user.csrf_ok(&csrf) {
                return Ok(csrf_rejection());
            }
            if reserved.is_some() || draft.media_id.is_some() {
                return Err(ApiError::BadRequest(
                    "Choose one video or audio file".into(),
                ));
            }
            let id = reserve_upload(state, user, headers, draft, None, None, None).await?;
            *reserved = Some(id);
            upload = spool_video(state, field, file_limit).await?;
            continue;
        }
        if reserved.is_some()
            && matches!(
                name.as_str(),
                "csrf"
                    | "channel"
                    | "title"
                    | "description"
                    | "language"
                    | "visibility"
                    | "poster_time"
                    | "media_id"
                    | "category"
                    | "licence"
                    | "spoiler_text"
                    | "sensitive"
            )
        {
            return Err(ApiError::BadRequest(
                "Video details must precede the file".into(),
            ));
        }
        let limit = if name == "captions" {
            super::actions::MAX_CAPTION_BYTES
        } else {
            64 * 1024
        };
        let bytes = bounded_field(field, limit).await?;
        if name == "captions" && !bytes.is_empty() {
            captions = super::actions::parse_caption_vtt(&bytes)?;
            continue;
        }
        let Ok(value) = String::from_utf8(bytes) else {
            return Err(ApiError::BadRequest("Invalid text encoding".into()));
        };
        match name.as_str() {
            "csrf" => csrf = value,
            "op" => operation = value,
            "channel" => draft.channel = value,
            "title" => draft.title = value,
            "description" => draft.description = value,
            "language" => draft.language = value,
            "visibility" => draft.visibility = value,
            "poster_time" => draft.poster_time = value,
            "scheduled_at" => draft.scheduled_at = value,
            "category" => draft.category = value,
            "licence" => draft.licence = value,
            "spoiler_text" => draft.spoiler_text = value,
            "sensitive" => draft.sensitive = crate::routes::params::truthy(Some(&value)),
            "alt" => draft.alt = value,
            "transcript" => draft.transcript = value,
            "media_id" => draft.media_id = value.parse().ok(),
            _ => {}
        }
    }
    if !user.csrf_ok(&csrf) {
        return Ok(csrf_rejection());
    }
    if let Some(id) = *reserved {
        let (file, length) = upload.ok_or_else(|| {
            ApiError::Unprocessable("Choose a non-empty video or audio file".into())
        })?;
        finish_upload(
            state.clone(),
            id,
            user.current.account.id,
            file,
            length,
            draft.alt.clone(),
            draft.transcript.clone(),
            captions,
        )
        .await?;
        return Ok(redirect_to(&format!("/videos/drafts/{id}")));
    }
    process(state, user, draft, &operation).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one recoverable private draft reservation and rollback"
)]
pub(super) async fn reserve_upload(
    state: &AppState,
    user: &WebUser,
    headers: &HeaderMap,
    draft: &Draft,
    exact_bytes: Option<u64>,
    fingerprint: Option<&str>,
    captions: Option<&str>,
) -> Result<i64, ApiError> {
    let channel_id: i64 = draft
        .channel
        .parse()
        .map_err(|_| ApiError::Unprocessable("Choose a video channel before the file".into()))?;
    if !(3..=120).contains(&draft.title.trim().chars().count()) {
        return Err(ApiError::Unprocessable(
            "Give the video a title of 3–120 characters".into(),
        ));
    }
    if draft.description.chars().count() > 10_000
        || draft.language.chars().count() > 35
        || !matches!(draft.visibility.as_str(), "public" | "unlisted")
    {
        return Err(ApiError::Unprocessable("Check the video details".into()));
    }
    let poster_time_seconds = if draft.poster_time.is_empty() {
        0.0
    } else {
        draft.poster_time.parse::<f64>().map_err(|_| {
            ApiError::Unprocessable("Choose a poster time from 0 to 86400 seconds".into())
        })?
    };
    if !poster_time_seconds.is_finite() || !(0.0..=86400.0).contains(&poster_time_seconds) {
        return Err(ApiError::Unprocessable(
            "Choose a poster time from 0 to 86400 seconds".into(),
        ));
    }
    let max = state.config.native_video.max_upload_bytes();
    if exact_bytes.is_some_and(|bytes| bytes == 0 || bytes > max) {
        return Err(ApiError::PayloadTooLarge);
    }
    if fingerprint.is_some_and(|value| {
        value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(ApiError::BadRequest("Invalid video fingerprint".into()));
    }
    // Content-Length includes multipart framing. A smaller advertised body
    // bounds the source; missing/chunked length reserves the full upload cap.
    let requested = exact_bytes.unwrap_or_else(|| {
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(max, |value| value.min(max))
    });
    let reserved_bytes = i64::try_from(requested.max(1)).unwrap_or(i64::MAX);
    let config = state.config.native_video;
    let id = id::next();
    let source_file = format!("{id}.orig");
    let available = crate::storage::available_bytes(&state.config.media_dir)
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let limits = video_draft::AdmissionLimits {
        unfinished_per_account: i64::from(config.unfinished_drafts),
        retained_source_bytes_per_account: config.storage_limits().per_account,
        pending_source_bytes_global: i64::try_from(
            config.pending_gib * crate::config::NativeVideoConfig::GIB,
        )
        .unwrap_or(i64::MAX),
        storage: config.storage_limits(),
        disk_available_bytes: available,
        min_free_bytes: config.min_free_gib * crate::config::NativeVideoConfig::GIB,
        processing_reserved_bytes: state.video_resources.usage().1,
    };
    let new = video_draft::NewDraft {
        media_id: id,
        account_id: user.current.account.id,
        channel_id,
        title: draft.title.trim(),
        description: draft.description.trim(),
        language: (!draft.language.is_empty()).then_some(draft.language.as_str()),
        visibility: &draft.visibility,
        source_file: &source_file,
        source_fingerprint: fingerprint,
        poster_time_seconds,
        media_description: (!draft.alt.is_empty()).then_some(draft.alt.as_str()),
        media_transcript: (!draft.transcript.is_empty()).then_some(draft.transcript.as_str()),
        caption_vtt: captions,
        reserved_bytes,
    };
    match video_draft::reserve(&state.pool, &new, &limits).await? {
        video_draft::Admission::Accepted => {
            if let Err(error) = save_options(state, user.current.account.id, id, draft).await {
                video_draft::cancel(&state.pool, id, user.current.account.id).await?;
                return Err(error);
            }
            Ok(id)
        },
        video_draft::Admission::Forbidden => Err(ApiError::Forbidden(
            "This is not your video channel".into(),
        )),
        video_draft::Admission::AccountFull => Err(ApiError::Unprocessable(
            format!(
                "You can have up to {} unfinished video drafts. Publish or cancel one first.",
                config.unfinished_drafts
            ),
        )),
        video_draft::Admission::StorageFull => Err(ApiError::Unprocessable(
            "Video storage is full or temporarily reserved. Try a smaller file or wait for cleanup.".into(),
        )),
    }
}

/// The finalizer owns the temporary file and row fence even if the browser
/// disconnects during the short rename/commit window.
#[allow(clippy::too_many_arguments)]
async fn finish_upload(
    state: AppState,
    id: i64,
    owner: i64,
    upload: tempfile::NamedTempFile,
    bytes: i64,
    alt: String,
    transcript: String,
    captions: Option<String>,
) -> Result<(), ApiError> {
    tokio::spawn(async move {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        if !video_draft::lock_receiving(&mut tx, id, owner).await? {
            return Err(ApiError::Conflict(
                "Video upload was cancelled or expired".into(),
            ));
        }
        state
            .media
            .put_file(&format!("{id}.orig"), upload.path())
            .await
            .map_err(|error| ApiError::Internal(Box::new(error)))?;
        sqlx::query!(
            "UPDATE media_attachments SET description = $3, transcript = $4,
            caption_vtt = $5 WHERE id = $1 AND account_id = $2 AND status_id IS NULL",
            id,
            owner,
            (!alt.is_empty()).then_some(alt.as_str()),
            (!transcript.is_empty()).then_some(transcript.as_str()),
            captions
        )
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
        if !video_draft::finish_receiving(&mut tx, id, owner, bytes).await? {
            return Err(ApiError::Conflict(
                "Video upload was cancelled or expired".into(),
            ));
        }
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Ok(())
    })
    .await
    .map_err(|error| ApiError::Internal(Box::new(error)))?
}

async fn process(
    state: &AppState,
    user: &WebUser,
    draft: &Draft,
    operation: &str,
) -> Result<Response, ApiError> {
    let channel_id: i64 = draft
        .channel
        .parse()
        .map_err(|_| ApiError::Unprocessable("Choose a channel".into()))?;
    if plamenu_db::video_channel_access::role(&state.pool, channel_id, user.current.account.id)
        .await?
        .is_none()
    {
        return Err(ApiError::Forbidden("This is not your channel".into()));
    }
    let id = draft
        .media_id
        .ok_or_else(|| ApiError::Unprocessable("Choose a video or audio file".into()))?;
    if operation != "publish" && operation != "schedule" {
        return Ok(page(state, user, draft, None).await?.into_response());
    }
    let metadata = save_options(state, user.current.account.id, id, draft).await?;
    if operation == "schedule" {
        let instant =
            super::scheduled::resolve_schedule_input(state, user, &draft.scheduled_at).await?;
        crate::routes::scheduled_statuses::create_scheduled_video(
            state,
            &user.current.account,
            instant,
            channel_id,
            id,
            &draft.title,
            &draft.description,
            &draft.visibility,
            (!draft.language.is_empty()).then_some(draft.language.as_str()),
            None,
        )
        .await?;
        return Ok(redirect_to("/settings/scheduled?saved=1"));
    }
    let (stored, _) = crate::actions::post_video(
        state,
        PostParams {
            username: &user.current.account.username,
            media_ids: &[id],
            spoiler_text: &metadata.spoiler_text,
            sensitive: metadata.sensitive,
            title: Some(&draft.title),
            text: &draft.description,
            language: (!draft.language.is_empty()).then_some(draft.language.as_str()),
            visibility: &draft.visibility,
            ..Default::default()
        },
        channel_id,
    )
    .await?;
    Ok(redirect_to(&format!("/web/statuses/{}", stored.id)))
}

async fn save_options(
    state: &AppState,
    owner: i64,
    id: i64,
    draft: &Draft,
) -> Result<plamenu_db::video_metadata::Metadata, ApiError> {
    crate::video_metadata::draft(
        state,
        owner,
        id,
        &crate::video_metadata::Input {
            category: Some(serde_json::Value::from(draft.category.as_str())),
            licence: Some(serde_json::Value::from(draft.licence.as_str())),
            spoiler_text: Some(draft.spoiler_text.clone()),
            sensitive: Some(serde_json::Value::from(draft.sensitive)),
        },
    )
    .await
}

/// Small metadata fields are bounded while reading, not after allocation.
pub(super) async fn bounded_field(
    mut field: axum::extract::multipart::Field<'_>,
    limit: usize,
) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(|_| ApiError::PayloadTooLarge)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ApiError::PayloadTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Only a multipart chunk is resident in memory. The private tempfile is
/// removed on validation failure, cancellation or successful processing.
async fn spool_video(
    state: &AppState,
    mut field: axum::extract::multipart::Field<'_>,
    limit: usize,
) -> Result<Option<(tempfile::NamedTempFile, i64)>, ApiError> {
    let root = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let spool = tempfile::Builder::new()
        .prefix("video-upload-")
        .tempfile_in(root)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut file = tokio::fs::File::from_std(
        spool
            .reopen()
            .map_err(|e| ApiError::Internal(Box::new(e)))?,
    );
    let mut length = 0usize;
    while let Some(chunk) = field.chunk().await.map_err(|_| ApiError::PayloadTooLarge)? {
        if chunk.len() > limit.saturating_sub(length) {
            return Err(ApiError::PayloadTooLarge);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        length += chunk.len();
    }
    file.flush()
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok((length > 0).then_some((spool, i64::try_from(length).unwrap_or(i64::MAX))))
}
