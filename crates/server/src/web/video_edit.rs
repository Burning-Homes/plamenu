//! Ordinary HTML private-candidate review and explicitly confirmed application.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError, video_edit};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;

#[derive(Default, Deserialize)]
pub struct Selection {
    candidate: Option<i64>,
}
fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}
pub async fn page(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    Query(selection): Query<Selection>,
) -> Response {
    private(
        render(&state, &user, id, selection.candidate, None)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
#[expect(
    clippy::too_many_lines,
    reason = "one HTML-first private editing and review template"
)]
async fn render(
    state: &AppState,
    user: &WebUser,
    id: i64,
    selected: Option<i64>,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let current = video_edit::editable(state, id, user.current.account.id).await?;
    let candidates = video_edit::candidates(state, id, user.current.account.id).await?;
    let selected = selected.filter(|id| candidates.iter().any(|c| c.media_id == *id));
    let preview = if let Some(candidate) = selected {
        plamenu_db::media::find_by_ids(&state.pool, &[candidate])
            .await?
            .pop()
    } else {
        None
    };
    let tracks = plamenu_db::video_captions::for_media(
        &state.pool,
        &selected.into_iter().collect::<Vec<_>>(),
    )
    .await?;
    let l = user.locale;
    let body = html! {
        section.column.video-edit.video-form {
            h1 {(l.text("video-edit-title"))}
            h2 {(current.title)}
            @if let Some(error)=error {p.settings__error role="alert" {(error)}}
            p {(l.text("video-edit-hint"))}
            (super::video_workspace::video_navigation(id,current.status_id,l))
            details.video-form__section.video-form__panel {
            summary {(l.text("video-edit-source-title"))}
            p id="video-edit-source-hint" {(l.text("video-edit-source-hint"))}
            form.settings-form method="post" action=(format!("/web/videos/{id}/prepare-edit")) {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="revision" value=(current.revision);
                input type="hidden" name="operation" value="reprocess";
                button type="submit" aria-describedby="video-edit-source-hint" {(l.text("video-edit-reprocess"))}
            }
            }
            details.video-form__section.video-form__panel {
                summary {(l.text("video-edit-trim"))}
                p id="video-edit-trim-hint" {(l.text("video-edit-trim-hint"))}
                form.settings-form method="post" action=(format!("/web/videos/{id}/prepare-edit")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="revision" value=(current.revision);
                    input type="hidden" name="operation" value="trim";
                    label.settings-field {span.settings-field__label {(l.text("video-edit-start"))}
                        input type="number" name="start" min="0" step="0.001" value="0" required aria-describedby="video-edit-trim-hint";
                    }
                    label.settings-field {span.settings-field__label {(l.text("video-edit-end"))}
                        input type="number" name="end" min="0.001" step="0.001" required aria-describedby="video-edit-trim-hint";
                    }
                    button type="submit" {(l.text("video-edit-prepare-trim"))}
                }
            }
            details.video-form__section.video-form__panel {
                summary {(l.text("studio-replace-film"))}
            ol {
                li {a href="/videos/upload" {(l.text("video-edit-upload"))} " · " a href="/videos/import" {(l.text("video-import-title"))}}
                li {(l.text("video-edit-prepare"))}
                li {(l.text("video-edit-apply-hint"))}
            }
            }
            @if candidates.is_empty() {p {(l.text("video-edit-empty"))}}
            @else {
                form.settings-form method="get" action=(format!("/videos/{id}/edit")) {
                    label.settings-field {span.settings-field__label {(l.text("video-edit-candidate"))}
                        select name="candidate" required {
                            option value="" selected[selected.is_none()] {(l.text("video-edit-select"))}
                            @for c in &candidates {option value=(c.media_id) selected[selected==Some(c.media_id)] {(c.title) " · " (c.media_id)}}
                        }
                    }
                    button type="submit" {(l.text("videos-review"))}
                }
            }
            @if let Some(candidate)=selected {
                h3 {(l.text("video-edit-review"))}
                video.video-publish__preview controls preload="metadata" playsinline aria-label=(l.text("video-edit-preview")) src=(format!("/videos/drafts/{candidate}/preview/video")) {
                    @if let Some(tracks)=tracks.get(&candidate) {
                        @for (index,track) in tracks.iter().enumerate() {
                            track kind="captions" srclang=(track.language) label=(track.language) default[index==0]
                                src=(if track.legacy {format!("/videos/drafts/{candidate}/preview/captions.vtt")}else{format!("/videos/drafts/{candidate}/captions/{}.vtt",track.language)});
                        }
                    }
                }
                @if let Some(preview)=&preview {
                    @if let Some(alt)=&preview.description {details {summary {(l.text("videos-alt"))} p {(alt)}}}
                    @if let Some(transcript)=&preview.transcript {details {summary {(l.text("videos-transcript"))} p {(transcript)}}}
                }
                p {a href=(format!("/videos/drafts/{candidate}/review")) {(l.text("video-edit-accessibility"))}}
                p id="video-edit-effect" {(l.text("video-edit-effect"))}
                form.settings-form method="post" action=(format!("/web/videos/{id}/edit")) {
                    input type="hidden" name="csrf" value=(user.csrf);
                    input type="hidden" name="candidate" value=(candidate);
                    input type="hidden" name="revision" value=(current.revision);
                    label.settings-field {span.settings-field__label {
                        input type="checkbox" name="confirm" value="yes" required aria-describedby="video-edit-effect";
                        " " (l.text("video-edit-confirm"))
                    }}
                    button type="submit" {(l.text("video-edit-apply"))}
                }
            }
            p {a href=(format!("/videos/{id}/metadata")) {(l.text("videos-metadata-manage"))}}
        }
    };
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        layout::shell(&l.text("video-edit-title"), Some(user), &body),
    )
        .into_response())
}
pub async fn prepare(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(
        prepare_inner(&state, &user, id, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
async fn prepare_inner(
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
    let result = async {
        let revision = field(&pairs, "revision")
            .and_then(|v| v.parse().ok())
            .ok_or(ApiError::Unprocessable(
                "Review the current film first".into(),
            ))?;
        let operation = match field(&pairs, "operation") {
            Some("reprocess") => crate::video_source_edit::Operation::Reprocess,
            Some("trim") => {
                let number = |key| {
                    field(&pairs, key)
                        .and_then(|v| v.parse().ok())
                        .ok_or(ApiError::Unprocessable(
                        "Choose start and end times in seconds, with up to three decimal places"
                            .into(),
                    ))
                };
                crate::video_source_edit::Operation::Trim(crate::video_clip::Window::from_seconds(
                    number("start")?,
                    number("end")?,
                )?)
            }
            _ => {
                return Err(ApiError::Unprocessable(
                    "Choose reprocessing or trimming".into(),
                ));
            }
        };
        let candidate = crate::video_source_edit::prepare(
            state,
            id,
            user.current.account.id,
            revision,
            operation,
        )
        .await?;
        Ok(redirect_to(&format!("/videos/drafts/{candidate}")))
    }
    .await;
    match result {
        Err(ApiError::Unprocessable(message)) => {
            render(state, user, id, None, Some(&message)).await
        }
        Err(ApiError::Conflict(message)) => {
            let mut response = render(state, user, id, None, Some(&message)).await?;
            *response.status_mut() = StatusCode::CONFLICT;
            Ok(response)
        }
        other => other,
    }
}
pub async fn apply(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Response {
    private(
        apply_inner(&state, &user, id, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}
async fn apply_inner(
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
    let candidate = field(&pairs, "candidate")
        .and_then(|s| s.parse().ok())
        .ok_or(ApiError::Unprocessable("Choose a replacement draft".into()))?;
    let revision = field(&pairs, "revision")
        .and_then(|s| s.parse().ok())
        .ok_or(ApiError::Unprocessable(
            "Review the current film first".into(),
        ))?;
    if field(&pairs, "confirm") != Some("yes") {
        return render(
            state,
            user,
            id,
            Some(candidate),
            Some(&user.locale.text("video-edit-confirm")),
        )
        .await;
    }
    let current = video_edit::editable(state, id, user.current.account.id).await?;
    match video_edit::replace(state, id, user.current.account.id, candidate, revision).await {
        Ok(()) => {}
        Err(ApiError::Conflict(message)) => {
            let mut response = render(state, user, id, Some(candidate), Some(&message)).await?;
            *response.status_mut() = StatusCode::CONFLICT;
            return Ok(response);
        }
        Err(ApiError::Unprocessable(message)) => {
            return render(state, user, id, Some(candidate), Some(&message)).await;
        }
        Err(error) => return Err(error),
    }
    Ok(redirect_to(&format!("/web/statuses/{}", current.status_id)))
}
