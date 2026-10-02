//! HTML-first timed chapter authoring for native Video drafts and publications.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use plamenu_db::{account, media, status, video_captions, video_chapters};

use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
    video_drafts,
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

fn clock(seconds: i32) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(super) fn lines(chapters: &[video_chapters::Chapter]) -> String {
    chapters
        .iter()
        .map(|chapter| format!("{} {}", clock(chapter.start_seconds), chapter.title))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_clock(value: &str) -> Option<i32> {
    let pieces = value.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match pieces.as_slice() {
        [minutes, seconds] => (
            0,
            minutes.parse::<i32>().ok()?,
            seconds.parse::<i32>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<i32>().ok()?,
            minutes.parse::<i32>().ok()?,
            seconds.parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    if hours < 0
        || minutes < 0
        || !(0..60).contains(&seconds)
        || (pieces.len() == 3 && minutes >= 60)
    {
        return None;
    }
    hours
        .checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?
        .checked_add(seconds)
}

fn parse_lines(
    source: &str,
    media_id: i64,
    duration: Option<f64>,
) -> Result<Vec<video_chapters::Chapter>, ApiError> {
    if source.len() > 32 * 1024 {
        return Err(ApiError::Unprocessable("Chapter list is too long".into()));
    }
    let mut chapters = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (stamp, title) = line.split_once(char::is_whitespace).ok_or_else(|| {
            ApiError::Unprocessable(format!("Chapter line {} needs a time and title", index + 1))
        })?;
        let title = title.trim();
        let start = parse_clock(stamp).ok_or_else(|| {
            ApiError::Unprocessable(format!("Chapter line {} has an invalid time", index + 1))
        })?;
        if !video_chapters::valid_chapter(start, title) {
            return Err(ApiError::Unprocessable(format!(
                "Chapter line {} needs a title of at most 200 characters and a valid time",
                index + 1
            )));
        }
        if duration.is_some_and(|seconds| seconds > 0.0 && f64::from(start) >= seconds.ceil()) {
            return Err(ApiError::Unprocessable(format!(
                "Chapter line {} starts after the video ends",
                index + 1
            )));
        }
        chapters.push(video_chapters::Chapter {
            media_id,
            start_seconds: start,
            title: title.to_owned(),
        });
        if chapters.len() > video_chapters::MAX_CHAPTERS {
            return Err(ApiError::Unprocessable(
                "A video can have at most 100 chapters".into(),
            ));
        }
    }
    chapters.sort_unstable_by_key(|chapter| chapter.start_seconds);
    if chapters
        .windows(2)
        .any(|pair| pair[0].start_seconds == pair[1].start_seconds)
    {
        return Err(ApiError::Unprocessable(
            "Each chapter needs a different start time".into(),
        ));
    }
    Ok(chapters)
}

pub(super) fn editor(
    media_id: i64,
    chapters: &[video_chapters::Chapter],
    csrf: &str,
    published: bool,
    locale: crate::web::i18n::Locale,
) -> Markup {
    let action = if published {
        format!("/web/videos/{media_id}/chapters")
    } else {
        format!("/web/videos/drafts/{media_id}/chapters")
    };
    html! {
        section.video-chapters id="chapter-editor" {
            h2 { (locale.text("videos-chapters-title")) }
            p.settings-field__hint { (locale.text("videos-chapters-hint")) }
            form.settings-form data-chapter-editor
                data-chapter-ready=(locale.text("videos-chapters-ready"))
                data-chapter-invalid=(locale.text("videos-chapters-invalid"))
                method="post" action=(action) {
                input type="hidden" name="csrf" value=(csrf);
                label.settings-field {
                    span.settings-field__label { (locale.text("videos-chapters-lines")) }
                    textarea name="chapters" rows="6" maxlength="32768"
                        placeholder=("0:00 Opening scene\n1:30 Next scene") { (lines(chapters)) }
                }
                p role="status" aria-live="polite" data-chapter-status hidden {}
                ol data-chapter-preview hidden {}
                button type="button" data-chapter-current hidden { (locale.text("videos-chapters-current")) }
                button type="submit" { (locale.text("videos-chapters-save")) }
            }
        }
    }
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
        .pop()
        .ok_or(ApiError::NotFound)?;
    let chapters = video_chapters::for_media(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let locale = user.locale;
    let body = html! {
        section.column.video-form {
            h1 { (locale.text("videos-chapters-published")) }
            h2 { (publication.title) }
            @if let Some(message) = error { p.settings__error role="alert" { (message) } }
            (super::video_workspace::video_navigation(id,publication.status_id,locale))
            @if let Some(file) = media.file_name.as_deref() {
                video.video-publish__preview controls preload="metadata" aria-label=(publication.title) {
                    source src=(format!("/media/{file}")) type=(media.content_type);
                }
            }
            (editor(id, &chapters, &user.csrf, true, locale))
        }
    };
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        layout::shell(&locale.text("videos-chapters-published"), Some(user), &body),
    )
        .into_response())
}

pub async fn draft_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let result = draft_update_inner(&state, &user, id, &body).await;
    let result = match result {
        Err(ApiError::Unprocessable(message)) => {
            video_drafts::review_inner(&state, &user, id, Some(&message)).await
        }
        other => other,
    };
    private(result)
}

async fn draft_update_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    let media = media::find_by_ids(&state.pool, &[id])
        .await?
        .into_iter()
        .find(|item| item.account_id == user.current.account.id && item.status_id.is_none())
        .ok_or(ApiError::NotFound)?;
    let chapters = parse_lines(
        field(&pairs, "chapters").unwrap_or_default(),
        id,
        media.duration,
    )?;
    if !video_chapters::replace_draft(&state.pool, id, user.current.account.id, &chapters).await? {
        return Err(ApiError::NotFound);
    }
    Ok(redirect_to(&format!(
        "/videos/drafts/{id}/review#chapter-editor"
    )))
}

pub async fn published_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    let result = published_update_inner(&state, &user, id, &body).await;
    let result = match result {
        Err(ApiError::Unprocessable(message)) => {
            manage_inner(&state, &user, id, Some(&message)).await
        }
        other => other,
    };
    private(result)
}

async fn published_update_inner(
    state: &AppState,
    user: &WebUser,
    id: i64,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    user.current.require_scope("write:media")?;
    user.current.require_scope("write:statuses")?;
    video_captions::owned_publication(&state.pool, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let source = media::find_by_ids(&state.pool, &[id])
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    let chapters = parse_lines(
        field(&pairs, "chapters").unwrap_or_default(),
        id,
        source.duration,
    )?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    let publication = video_captions::lock_owned_publication(&mut tx, id, user.current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    video_chapters::replace_conn(&mut tx, id, &chapters).await?;
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
    Ok(redirect_to(&format!(
        "/videos/{id}/chapters#chapter-editor"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_lines_are_bounded_and_sorted() {
        let parsed = parse_lines("1:30 Second\n0:00 Opening\n", 7, Some(120.0)).unwrap();
        assert_eq!(lines(&parsed), "0:00 Opening\n1:30 Second");
        assert!(parse_lines("0:00 One\n0:00 Duplicate", 7, None).is_err());
        assert!(parse_lines("0:60 Bad", 7, None).is_err());
        assert!(parse_lines("2:00 Beyond duration", 7, Some(120.0)).is_err());
        assert!(parse_lines("", 7, Some(120.0)).unwrap().is_empty());
    }
}
