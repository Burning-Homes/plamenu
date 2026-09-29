//! HTML-first draft recovery. GETs never enqueue, retry, cancel or publish.
use axum::{
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{media, video_draft};

use super::{
    layout,
    session::WebUser,
    settings::{field, form_pairs, redirect_to},
    videos,
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
    response
}

async fn owned(
    state: &AppState,
    id: i64,
    owner: i64,
) -> Result<(video_draft::Draft, media::Media), ApiError> {
    let draft = video_draft::find_owned(&state.pool, id, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    let media = media::find_by_ids(&state.pool, &[id])
        .await?
        .into_iter()
        .find(|m| m.account_id == owner && m.status_id.is_none())
        .ok_or(ApiError::NotFound)?;
    Ok((draft, media))
}

async fn scheduled_reservation(
    state: &AppState,
    media_id: i64,
    owner: i64,
) -> Result<Option<i64>, ApiError> {
    sqlx::query_scalar(
        "SELECT scheduled_status_id FROM media_attachments WHERE id = $1 AND account_id = $2",
    )
    .bind(media_id)
    .bind(owner)
    .fetch_one(&state.pool)
    .await
    .map_err(plamenu_db::DbError::from)
    .map_err(ApiError::from)
}

pub async fn show(State(state): State<AppState>, Path(id): Path<i64>, user: WebUser) -> Response {
    private(show_inner(&state, &user, id, false).await)
}

/// The enhancement reads only the phase. Cookie authorization and private
/// caching match the HTML view; no mutation or storage key is exposed here.
pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    private(status_inner(&state, id, &headers).await)
}

async fn status_inner(
    state: &AppState,
    id: i64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let token = super::session::raw_session_token(headers).ok_or(ApiError::NotFound)?;
    let user = crate::auth::user_for_token(state, token)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let (draft, _) = owned(state, id, user.account.id).await?;
    let phase = if draft.expires_at <= time::OffsetDateTime::now_utc() {
        "expired"
    } else if draft.state == "ready"
        && video_draft::preview(&state.pool, id, user.account.id)
            .await?
            .is_none()
    {
        "unavailable"
    } else {
        draft.state.as_str()
    };
    Ok(axum::Json(serde_json::json!({"phase": phase})).into_response())
}

async fn show_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    conflict: bool,
) -> Result<Response, ApiError> {
    let (draft, _) = owned(state, id, user.current.account.id).await?;
    let locale = user.locale;
    let scheduled = scheduled_reservation(state, id, user.current.account.id)
        .await?
        .is_some();
    let expired = draft.expires_at <= time::OffsetDateTime::now_utc();
    let phase = if expired {
        "expired"
    } else {
        draft.state.as_str()
    };
    let ready = phase == "ready"
        && video_draft::preview(&state.pool, id, user.current.account.id)
            .await?
            .is_some();
    let phase = if phase == "ready" && !ready {
        "unavailable"
    } else {
        phase
    };
    let body = html! {
        section.column.video-draft data-video-draft=(id) data-video-scheduled[scheduled] data-phase=(phase)
            data-phase-receiving=(locale.text("videos-draft-receiving"))
            data-phase-queued=(locale.text("videos-draft-queued"))
            data-phase-processing=(locale.text("videos-draft-processing"))
            data-phase-ready=(locale.text("videos-draft-ready"))
            data-phase-failed=(locale.text("videos-draft-failed"))
            data-phase-expired=(locale.text("videos-draft-expired"))
            data-phase-unavailable=(locale.text("videos-draft-unavailable")) {
            h1 { (locale.text("videos-draft-title")) }
            h2 { (draft.title) }
            @if conflict { p.settings__error role="alert" { (locale.text("videos-draft-changed")) } }
            p { (locale.text("videos-draft-private")) }
            @if scheduled {
                p { (locale.text("videos-draft-scheduled")) " " a href="/settings/scheduled" { (locale.text("compose-scheduled-posts")) } }
            }
            p role="status" aria-live="polite" { (locale.text(&format!("videos-draft-{phase}"))) }
            @if phase == "receiving" {
                p { (locale.text("videos-draft-receiving-help")) }
                @if draft.source_fingerprint.is_some() {
                    form.settings-form data-video-resume
                        data-mismatch=(locale.text("videos-draft-file-mismatch"))
                        data-failed=(locale.text("videos-upload-failed")) hidden {
                        input type="hidden" name="csrf" value=(user.csrf);
                        label.settings-field {
                            span.settings-field__label { (locale.text("videos-draft-resume-file")) }
                            input type="file" name="file" accept="video/*,audio/*" required;
                        }
                        p.settings-field__hint { (locale.text("videos-draft-resume-hint")) }
                        div.video-upload__progress data-video-upload-progress hidden
                            data-uploading=(locale.text("videos-uploading"))
                            data-saving=(locale.text("videos-upload-saving")) {
                            p role="status" aria-live="polite" data-video-upload-label { (locale.text("videos-uploading")) " 0%" }
                            progress max="100" value="0" aria-label=(locale.text("videos-upload-progress")) data-video-upload-meter;
                        }
                        p.settings__error role="alert" tabindex="-1" data-video-upload-error hidden {}
                        button type="submit" { (locale.text("videos-draft-resume")) }
                    }
                }
            }
            @if !scheduled { p { (locale.text("videos-draft-expiry")) } }
            p data-draft-expiry { (locale.text("videos-draft-expires-at")) " " (user.clock.element_absolute(draft.expires_at)) }
            (super::clock::zone_chip(&user.clock))
            div.settings-form__actions {
                a href=(format!("/videos/drafts/{id}")) { (locale.text("videos-draft-refresh")) }
                a data-video-review hidden[!ready || scheduled] href=(format!("/videos/drafts/{id}/review")) { (locale.text("videos-review")) }
                a href="/videos" { (locale.text("videos-title")) }
            }
            form.settings-form data-video-retry hidden[phase != "failed" || scheduled] method="post" action=(format!("/web/videos/drafts/{id}")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    button type="submit" name="op" value="retry" { (locale.text("videos-draft-retry")) }
            }
            @if !scheduled { form.settings-form method="post" action=(format!("/web/videos/drafts/{id}")) {
                input type="hidden" name="csrf" value=(user.csrf);
                p id="draft-cancel-help" { (locale.text("videos-draft-cancel-help")) }
                label.settings-field {
                    span.settings-field__label {
                        input type="checkbox" name="confirm" value="yes" required aria-describedby="draft-cancel-help";
                        " " (locale.text("videos-draft-cancel-confirm"))
                    }
                }
                button.settings-button--danger type="submit" name="op" value="cancel" { (locale.text("videos-draft-cancel")) }
            } }
        }
    };
    Ok((
        if conflict {
            StatusCode::CONFLICT
        } else {
            StatusCode::OK
        },
        layout::shell(&locale.text("videos-draft-title"), Some(user), &body),
    )
        .into_response())
}

pub async fn review(State(state): State<AppState>, Path(id): Path<i64>, user: WebUser) -> Response {
    private(review_inner(&state, &user, id, None).await)
}

pub(super) async fn review_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let (draft, media) = owned(state, id, user.current.account.id).await?;
    if scheduled_reservation(state, id, user.current.account.id)
        .await?
        .is_some()
    {
        return Ok(redirect_to("/settings/scheduled"));
    }
    if video_draft::preview(&state.pool, id, user.current.account.id)
        .await?
        .is_none()
    {
        return Ok(redirect_to(&format!("/videos/drafts/{id}")));
    }
    let metadata = plamenu_db::video_metadata::for_media(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let draft = videos::Draft {
        channel: draft.channel_id.to_string(),
        title: draft.title,
        description: draft.description,
        language: draft.language.unwrap_or_default(),
        visibility: draft.visibility,
        alt: media.description.unwrap_or_default(),
        transcript: media.transcript.unwrap_or_default(),
        visual_audio: if media.audio_described {
            "audio_description"
        } else if media.visuals_conveyed_in_audio {
            "soundtrack"
        } else {
            ""
        }
        .to_owned(),
        poster_time: draft.poster_time_seconds.to_string(),
        poster_alt: draft.poster_alt,
        media_id: Some(id),
        scheduled_at: String::new(),
        category: metadata
            .category
            .map(|id| id.to_string())
            .unwrap_or_default(),
        licence: metadata
            .licence
            .map(|id| id.to_string())
            .unwrap_or_default(),
        spoiler_text: metadata.spoiler_text,
        sensitive: metadata.sensitive,
    };
    let page = videos::page(state, user, &draft, error).await?;
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        page,
    )
        .into_response())
}

pub async fn replace_poster(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    multipart: Multipart,
) -> Response {
    let result = replace_poster_inner(&state, &user, id, multipart).await;
    let result = match result {
        Err(ApiError::Unprocessable(message)) => {
            review_inner(&state, &user, id, Some(&message)).await
        }
        Err(ApiError::PayloadTooLarge) => {
            review_inner(
                &state,
                &user,
                id,
                Some("Choose a poster image smaller than 8 MiB"),
            )
            .await
        }
        other => other,
    };
    private(result)
}

async fn replace_poster_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    owned(state, id, user.current.account.id).await?;
    video_draft::preview(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut csrf = String::new();
    let mut alt = String::new();
    let mut image = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "poster_file" {
            if !user.csrf_ok(&csrf) {
                return Ok(super::session::csrf_rejection());
            }
            if image.is_some() {
                return Err(ApiError::BadRequest("Choose one poster image".into()));
            }
            image =
                Some(videos::bounded_field(field, crate::video_poster::MAX_UPLOAD_BYTES).await?);
        } else {
            if image.is_some() {
                return Err(ApiError::BadRequest(
                    "Poster details must precede the image".into(),
                ));
            }
            let value = videos::bounded_field(field, 2048).await?;
            let value = String::from_utf8(value)
                .map_err(|_| ApiError::BadRequest("Invalid poster details".into()))?;
            match name.as_str() {
                "csrf" => csrf = value,
                "poster_alt" => alt = value,
                _ => {}
            }
        }
    }
    if !user.csrf_ok(&csrf) {
        return Ok(super::session::csrf_rejection());
    }
    let image = image.ok_or_else(|| ApiError::Unprocessable("Choose a poster image".into()))?;
    crate::video_poster::replace_draft_poster(state, id, user.current.account.id, image, alt)
        .await?;
    Ok(redirect_to(&format!("/videos/drafts/{id}/review")))
}

pub async fn action(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(action_inner(&state, &user, id, &body).await)
}

pub async fn add_caption(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    multipart: Multipart,
) -> Response {
    let result = add_caption_inner(&state, &user, id, multipart).await;
    let result = match result {
        Err(ApiError::Unprocessable(message)) => {
            review_inner(&state, &user, id, Some(&message)).await
        }
        Err(ApiError::PayloadTooLarge) => {
            review_inner(&state, &user, id, Some("Captions must be at most 256 KiB")).await
        }
        other => other,
    };
    private(result)
}

async fn add_caption_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    owned(state, id, user.current.account.id).await?;
    video_draft::preview(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut csrf = String::new();
    let mut language = String::new();
    let mut body = None;
    while let Some(part) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?
    {
        match part.name().unwrap_or_default() {
            "csrf" => csrf = part.text().await.map_err(|_| ApiError::PayloadTooLarge)?,
            "language" => {
                language = part
                    .text()
                    .await
                    .map_err(|_| ApiError::PayloadTooLarge)?
                    .trim()
                    .to_ascii_lowercase();
            }
            "caption" => {
                if body.is_some() {
                    return Err(ApiError::Unprocessable("Choose one WebVTT file".into()));
                }
                body = Some(videos::bounded_field(part, super::actions::MAX_CAPTION_BYTES).await?);
            }
            _ => return Err(ApiError::BadRequest("Unexpected caption field".into())),
        }
    }
    if !user.csrf_ok(&csrf) {
        return Ok(super::session::csrf_rejection());
    }
    if !plamenu_db::video_captions::valid_language(&language) {
        return Err(ApiError::Unprocessable(
            "Choose a valid caption language".into(),
        ));
    }
    let body = super::actions::parse_caption_vtt(body.as_deref().unwrap_or_default())?
        .ok_or_else(|| ApiError::Unprocessable("Choose a WebVTT caption file".into()))?;
    if !plamenu_db::video_captions::upsert_draft(
        &state.pool,
        id,
        user.current.account.id,
        &language,
        &body,
    )
    .await?
    {
        return Err(ApiError::Unprocessable(
            "This draft is no longer ready, or its 16 caption languages are full".into(),
        ));
    }
    Ok(redirect_to(&format!(
        "/videos/drafts/{id}/review#caption-languages"
    )))
}

pub async fn remove_caption(
    State(state): State<AppState>,
    Path((id, language)): Path<(i64, String)>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(remove_caption_inner(&state, &user, id, &language, &body).await)
}

async fn remove_caption_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    language: &str,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(super::session::csrf_rejection());
    }
    if !plamenu_db::video_captions::valid_language(language)
        || !plamenu_db::video_captions::remove_draft(
            &state.pool,
            id,
            user.current.account.id,
            language,
        )
        .await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(redirect_to(&format!(
        "/videos/drafts/{id}/review#caption-languages"
    )))
}

async fn action_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(super::session::csrf_rejection());
    }
    owned(state, id, user.current.account.id).await?;
    match field(&pairs, "op") {
        Some("retry") if video_draft::retry(&state.pool, id, user.current.account.id).await? => {
            Ok(redirect_to(&format!("/videos/drafts/{id}")))
        }
        Some("cancel") if field(&pairs, "confirm") == Some("yes") => {
            if video_draft::cancel(&state.pool, id, user.current.account.id).await? {
                Ok(redirect_to("/videos"))
            } else {
                show_inner(state, user, id, true).await
            }
        }
        _ => show_inner(state, user, id, true).await,
    }
}
