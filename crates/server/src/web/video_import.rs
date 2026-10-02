//! HTML-first, explicitly authorized copies into the native private pipeline.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
    videos,
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::{account, video_draft, video_source_import};

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}

pub async fn page(State(state): State<AppState>, user: WebUser) -> Response {
    private(
        render(
            &state,
            &user,
            &videos::Draft {
                visibility: "public".into(),
                ..Default::default()
            },
            "",
            "file",
            None,
        )
        .await
        .unwrap_or_else(IntoResponse::into_response),
    )
}

async fn render(
    state: &AppState,
    user: &WebUser,
    draft: &videos::Draft,
    url: &str,
    kind: &str,
    error: Option<&str>,
) -> Result<Response, ApiError> {
    let available =
        plamenu_db::video_channel_access::available(&state.pool, user.current.account.id).await?;
    let channels = account::find_by_ids(
        &state.pool,
        &available.iter().map(|c| c.account_id).collect::<Vec<_>>(),
    )
    .await?;
    let locale = user.locale;
    let body = html! {
        section.column.video-form {
            div.video-form__heading {
                h1 { (locale.text("video-import-title")) }
            }
            p.video-form__intro { (locale.text("video-import-hint")) }
            (super::video_workspace::navigation("/videos",locale))
            @if let Some(error) = error { p.settings__error role="alert" { (error) } }
            @if channels.is_empty() {
                p { a href="/videos/channels/new" { (locale.text("videos-new-channel")) } }
            } @else {
                form.settings-form method="post" action="/web/videos/import" {
                    input type="hidden" name="csrf" value=(user.csrf);
                    label.settings-field { span.settings-field__label { (locale.text("videos-channel")) }
                        select name="channel" required {
                            @for channel in &channels { option value=(channel.id) selected[draft.channel == channel.id.to_string()] { (channel.display_name) } }
                        }
                    }
                    label.settings-field { span.settings-field__label { (locale.text("video-import-source-kind")) }
                        select name="source_kind" {
                            option value="file" selected[kind == "file"] { (locale.text("video-import-file")) }
                            option value="activitypub" selected[kind == "activitypub"] { (locale.text("video-import-activitypub")) }
                        }
                    }
                    label.settings-field { span.settings-field__label { (locale.text("video-import-url")) }
                        input type="url" name="source_url" maxlength="2048" required value=(url) aria-describedby="video-import-privacy";
                    }
                    p.settings-field__hint id="video-import-privacy" { (locale.text("video-import-privacy")) }
                    label.settings-field { span.settings-field__label { (locale.text("videos-name")) }
                        input name="title" minlength="3" maxlength="120" required value=(draft.title);
                    }
                    label.settings-field { span.settings-field__label { (locale.text("compose-visibility")) }
                        select name="visibility" {
                            option value="public" selected[draft.visibility == "public"] { (locale.text("visibility-public")) }
                            option value="unlisted" selected[draft.visibility == "unlisted"] { (locale.text("visibility-unlisted")) }
                        }
                    }
                    details.video-form__section.video-form__panel open[error.is_some()] {
                        summary {(locale.text("studio-options"))}
                        div.video-form__section-body {
                    label.settings-field { span.settings-field__label { (locale.text("videos-description")) }
                        textarea name="description" aria-label=(locale.text("videos-description")) maxlength="10000" rows="4" { (draft.description) }
                    }
                    (super::video_workspace::language_field("videos-language", &draft.language, false, "import", locale))
                    label.settings-field { span.settings-field__label { (locale.text("videos-alt")) }
                        textarea name="alt" aria-label=(locale.text("videos-alt")) maxlength="1500" rows="3" { (draft.alt) }
                    }
                    label.settings-field { span.settings-field__label { (locale.text("videos-transcript")) }
                        textarea name="transcript" aria-label=(locale.text("videos-transcript")) maxlength="50000" rows="4" { (draft.transcript) }
                    }
                    (super::video_metadata::fields(&draft.category, &draft.licence, locale))
                        }
                    }
                    label.settings-field { span.settings-field__label {
                        input type="checkbox" name="confirm" value="yes" required;
                        " " (locale.text("video-import-rights"))
                    } }
                    button type="submit" { (locale.text("video-import-submit")) }
                }
            }
            p { a href="/videos" { (locale.text("videos-title")) } }
        }
    };
    Ok((
        if error.is_some() {
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        } else {
            axum::http::StatusCode::OK
        },
        layout::shell(&locale.text("video-import-title"), Some(user), &body),
    )
        .into_response())
}

pub async fn submit(State(state): State<AppState>, user: WebUser, body: Bytes) -> Response {
    private(
        submit_inner(&state, &user, &body)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    )
}

async fn submit_inner(
    state: &AppState,
    user: &WebUser,
    body: &Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    // Cookie transport must not upgrade a granular/read-only OAuth token into
    // permission to allocate storage or make a server-side source request.
    user.current.require_scope("write:media")?;
    let value = |key| field(&pairs, key).unwrap_or_default().to_owned();
    let draft = videos::Draft {
        channel: value("channel"),
        title: value("title"),
        description: value("description"),
        language: value("language"),
        visibility: value("visibility"),
        alt: value("alt"),
        transcript: value("transcript"),
        category: value("category"),
        licence: value("licence"),
        ..Default::default()
    };
    let source = value("source_url");
    let kind = value("source_kind");
    let result = async {
        if value("confirm") != "yes" || !matches!(kind.as_str(), "file" | "activitypub") {
            return Err(ApiError::Unprocessable(
                user.locale.text("video-import-rights"),
            ));
        }
        let url =
            crate::video_source_import::validate_url(&source, state.config.allow_private_fetch)?;
        // A URL form's tiny Content-Length is NOT the remote file's size.
        let id = videos::reserve_upload(state, user, &HeaderMap::new(), &draft, None, None, None)
            .await?;
        match video_source_import::enqueue(&state.pool, id, user.current.account.id, &url, &kind)
            .await
        {
            Ok(true) => Ok(redirect_to(&format!("/videos/drafts/{id}"))),
            result => {
                video_draft::cancel(&state.pool, id, user.current.account.id).await?;
                result?;
                Err(ApiError::Conflict("Import draft was cancelled".into()))
            }
        }
    }
    .await;
    match result {
        Err(ApiError::Unprocessable(message) | ApiError::Forbidden(message)) => {
            render(state, user, &draft, &source, &kind, Some(&message)).await
        }
        other => other,
    }
}
