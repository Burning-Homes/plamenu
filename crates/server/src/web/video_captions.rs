//! HTML-first caption management for an already published native Video.

use axum::{
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{account, media, status, video_captions};

use super::{
    layout,
    session::{WebUser, csrf_rejection},
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

pub async fn manage(State(state): State<AppState>, Path(id): Path<i64>, user: WebUser) -> Response {
    private(manage_inner(&state, &user, id, None).await)
}

async fn manage_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let publication = video_captions::owned_publication(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let media = media::find_by_ids(&state.pool, &[id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    let tracks = video_captions::for_media(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let locale = user.locale;
    let watch_url = format!("/web/statuses/{}", publication.status_id);
    let body = html! {
        section.column.video-captions id="caption-languages" {
            h1 { (locale.text("videos-caption-published-title")) }
            h2 { (publication.title) }
            @if let Some(message) = error { p.settings__error role="alert" { (message) } }
            p.settings-field__hint { (locale.text("videos-caption-published-hint")) }
            nav aria-label=(locale.text("videos-caption-published-title")) {
                a href=(watch_url) { (locale.text("videos-caption-watch")) }
                " · "
                a href="/videos" { (locale.text("videos-title")) }
            }
            @if let Some(file) = media.file_name.as_deref() {
                video.video-publish__preview controls preload="metadata" aria-label=(publication.title) {
                    source src=(format!("/media/{file}")) type=(media.content_type);
                    @for (index, track) in tracks.iter().enumerate() {
                        track kind="captions"
                            src=(crate::video::caption_public_url(&state.config.domain, id, track))
                            srclang=(track.language) label=(track.language) default[index == 0];
                    }
                }
            }
            @if tracks.is_empty() {
                p { (locale.text("videos-caption-none")) }
            } @else {
                ul {
                    @for track in &tracks {
                        li {
                            (track.language) " — "
                            a href=(crate::video::caption_public_url(&state.config.domain, id, track)) {
                                (locale.text("videos-caption-open"))
                            }
                            form method="post"
                                action=(format!("/web/videos/{id}/captions/{}/remove", track.language)) {
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
                method="post" action=(format!("/web/videos/{id}/captions")) enctype="multipart/form-data" {
                input type="hidden" name="csrf" value=(user.csrf);
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-caption-language")) }
                    input name="language" required maxlength="35"
                        pattern="[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*" placeholder="en";
                }
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-captions")) }
                    input type="file" name="caption" accept=".vtt,text/vtt" required;
                }
                p role="status" aria-live="polite" data-caption-status hidden {}
                button type="submit" { (locale.text("videos-caption-save")) }
            }
        }
    };
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        layout::shell(
            &locale.text("videos-caption-published-title"),
            Some(user),
            &body,
        ),
    )
        .into_response())
}

pub async fn add(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    multipart: Multipart,
) -> Response {
    let result = add_inner(&state, &user, id, multipart).await;
    let result = match result {
        Err(ApiError::Unprocessable(message)) => {
            manage_inner(&state, &user, id, Some(&message)).await
        }
        Err(ApiError::PayloadTooLarge) => {
            manage_inner(&state, &user, id, Some("Captions must be at most 256 KiB")).await
        }
        other => other,
    };
    private(result)
}

async fn add_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:media")?;
    user.current.require_scope("write:statuses")?;
    video_captions::owned_publication(&state.pool, id, user.current.account.id)
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
        return Ok(csrf_rejection());
    }
    if !video_captions::valid_language(&language) {
        return Err(ApiError::Unprocessable(
            "Choose a valid caption language".into(),
        ));
    }
    let body = super::actions::parse_caption_vtt(body.as_deref().unwrap_or_default())?
        .ok_or_else(|| ApiError::Unprocessable("Choose a WebVTT caption file".into()))?;
    mutate(state, user, id, &language, Some(&body)).await?;
    Ok(redirect_to(&format!(
        "/videos/{id}/captions#caption-languages"
    )))
}

pub async fn remove(
    State(state): State<AppState>,
    Path((id, language)): Path<(i64, String)>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(remove_inner(&state, &user, id, &language, &body).await)
}

async fn remove_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    language: &str,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    user.current.require_scope("write:statuses")?;
    if !video_captions::valid_language(language) {
        return Err(ApiError::NotFound);
    }
    mutate(state, user, id, language, None).await?;
    Ok(redirect_to(&format!(
        "/videos/{id}/captions#caption-languages"
    )))
}

/// The caption row, Video's edited time and queued signed `Update(Video)` are
/// one transaction. A failed delivery preparation cannot leave only one side
/// committed; the ordinary outbox worker retries transport after commit.
async fn mutate(
    state: &AppState,
    user: &WebUser,
    media_id: i64,
    language: &str,
    replacement: Option<&str>,
) -> Result<(), ApiError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let publication =
        video_captions::lock_owned_publication(&mut tx, media_id, user.current.account.id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let changed = if let Some(body) = replacement {
        video_captions::upsert_published(&mut tx, media_id, language, body).await?
    } else {
        video_captions::remove_published(&mut tx, media_id, language).await?
    };
    if !changed {
        return Err(ApiError::Unprocessable(
            "The 16 caption languages are full, or this language is not attached".into(),
        ));
    }
    let edited_at = time::OffsetDateTime::now_utc();
    sqlx::query("UPDATE statuses SET edited_at = $2, updated_at = $2 WHERE id = $1")
        .bind(publication.status_id)
        .bind(edited_at)
        .execute(&mut *tx)
        .await
        .map_err(plamenu_db::DbError::from)?;
    let updated = status::find_by_id(&mut *tx, publication.status_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let actor = account::find_by_id(&mut *tx, publication.actor_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    crate::actions::federate_note_update_conn(
        state,
        &mut tx,
        &actor,
        &updated,
        edited_at,
        Vec::new(),
    )
    .await?;
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    crate::streaming::status_edited(state, updated.id).await;
    crate::webhooks::status_event(state, plamenu_db::webhook::STATUS_UPDATED, &updated).await;
    Ok(())
}
