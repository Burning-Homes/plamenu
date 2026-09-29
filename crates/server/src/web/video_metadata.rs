//! HTML-first category and licence management for published native Videos.
use super::{
    i18n::Locale,
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError};
use axum::{
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};

pub(super) fn fields(category: &str, licence: &str, locale: Locale) -> Markup {
    let selected = licence
        .parse::<i32>()
        .ok()
        .filter(|id| (1..=9).contains(id));
    let selected_url = selected.and_then(crate::video_metadata::licence_url);
    html! {
        label.settings-field {
            span.settings-field__label { (locale.text("videos-category")) }
            select name="category" {
                option value="0" selected[category.is_empty() || category == "0"] { (locale.text("videos-unspecified")) }
                @for id in 1..=18 {
                    option value=(id) selected[category == id.to_string()] { (locale.text(&format!("videos-category-{id}"))) }
                }
            }
        }
        label.settings-field {
            span.settings-field__label { (locale.text("videos-licence")) }
            select name="licence" data-video-licence aria-describedby="video-licence-hint" {
                option value="0" selected[licence.is_empty() || licence == "0"] { (locale.text("videos-unspecified")) }
                @for (index, name) in crate::video_metadata::LICENCES.iter().enumerate() {
                    @let id = i32::try_from(index + 1).unwrap_or(9);
                    option value=(id) selected[licence == id.to_string()]
                        data-licence-url=[crate::video_metadata::licence_url(id)] {
                        @if id == 9 { (locale.text("videos-all-rights-reserved")) } @else { (name) }
                    }
                }
            }
        }
        p.settings-field__hint id="video-licence-hint" {
            (locale.text("videos-licence-hint")) " "
            a href="https://creativecommons.org/share-your-work/" target="_blank" rel="noopener noreferrer" { (locale.text("videos-licence-learn")) }
        }
        p role="status" aria-live="polite" data-video-licence-status hidden[selected_url.is_none()] {
            a data-video-licence-link href=[selected_url] target="_blank" rel="noopener noreferrer" {
                @if let Some(id) = selected { (crate::video_metadata::LICENCES[usize::try_from(id - 1).unwrap_or(0)]) }
            }
        }
    }
}

pub async fn manage(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    private(
        render(
            &state,
            &user,
            id,
            None,
            query.get("saved").is_some_and(|value| value == "1"),
        )
        .await
        .unwrap_or_else(IntoResponse::into_response),
    )
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
async fn render(
    state: &AppState,
    user: &WebUser,
    id: i64,
    error: Option<&str>,
    saved: bool,
) -> Result<Response, ApiError> {
    let publication =
        plamenu_db::video_captions::owned_publication(&state.pool, id, user.current.account.id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let editable = !crate::video_edit::ready_native_ids(state, &[id])
        .await?
        .is_empty();
    let value = plamenu_db::video_metadata::for_media(&state.pool, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default();
    let body = html! {
        section.column {
            h1 { (user.locale.text("videos-metadata-manage")) }
            h2 { (publication.title) }
            @if saved { p.settings__saved role="status" { (user.locale.text("videos-metadata-saved")) } }
            @if let Some(error) = error { p.settings__error role="alert" { (error) } }
            p { a href=(format!("/web/statuses/{}", publication.status_id)) { (user.locale.text("videos-caption-watch")) } }
            p { a href=(format!("/web/statuses/{}/edit",publication.status_id)) { (user.locale.text("status-edit")) }
                " · " a href=(format!("/videos/{id}/captions")) { (user.locale.text("videos-caption-published-title")) }
                " · " a href=(format!("/videos/{id}/chapters")) { (user.locale.text("videos-chapters-title")) }
                @if editable { " · " a href=(format!("/videos/{id}/edit")) { (user.locale.text("video-edit-title")) } }
            }
            form.settings-form method="post" action=(format!("/web/videos/{id}/metadata")) {
                input type="hidden" name="csrf" value=(user.csrf);
                (fields(&value.category.map(|id| id.to_string()).unwrap_or_default(), &value.licence.map(|id| id.to_string()).unwrap_or_default(), user.locale))
                button type="submit" { (user.locale.text("videos-metadata-save")) }
            }
            (super::videos::replace_poster_form(id, &user.csrf, user.locale, true))
            @let delete_action=format!("/web/statuses/{}/delete",publication.status_id);
            @let delete_message=user.locale.text("status-delete-confirm");
            form.settings-form method="post" action=(super::view::CONFIRM_PATH) data-confirm=(&delete_message) data-confirm-action=(&delete_action) {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="return_to" value="/videos";
                (super::view::confirmation_fields(&delete_action,Some(&delete_message)))
                button.settings-button--danger type="submit" { (user.locale.text("status-delete")) }
            }
        }
    };
    Ok((
        if error.is_some() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::OK
        },
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(
            &user.locale.text("videos-metadata-manage"),
            Some(user),
            &body,
        ),
    )
        .into_response())
}
pub async fn save(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let pairs = match form_pairs(&body) {
        Ok(pairs) => pairs,
        Err(error) => return private(ApiError::BadRequest(error).into_response()),
    };
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return private(csrf_rejection());
    }
    let input = crate::video_metadata::Input {
        category: field(&pairs, "category").map(serde_json::Value::from),
        licence: field(&pairs, "licence").map(serde_json::Value::from),
        ..Default::default()
    };
    private(
        match crate::video_metadata::update(&state, user.current.account.id, id, &input).await {
            Ok(()) => redirect_to(&format!("/videos/{id}/metadata?saved=1")),
            Err(ApiError::Unprocessable(error)) => render(&state, &user, id, Some(&error), false)
                .await
                .unwrap_or_else(IntoResponse::into_response),
            Err(error) => error.into_response(),
        },
    )
}

pub async fn poster(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    multipart: Multipart,
) -> Response {
    let attempt = async {
        plamenu_db::video_captions::owned_publication(&state.pool, id, user.current.account.id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let Some((image, alt)) = super::video_drafts::poster_fields(&user, multipart).await? else {
            return Ok(csrf_rejection());
        };
        crate::video_poster::replace_published_poster(
            &state,
            id,
            user.current.account.id,
            image,
            alt,
        )
        .await?;
        Ok::<_, ApiError>(redirect_to(&format!(
            "/videos/{id}/metadata?saved=1#video-poster-replace"
        )))
    }
    .await;
    private(match attempt {
        Ok(response) => response,
        Err(ApiError::Unprocessable(message)) => render(&state, &user, id, Some(&message), false)
            .await
            .unwrap_or_else(IntoResponse::into_response),
        Err(ApiError::PayloadTooLarge) => render(
            &state,
            &user,
            id,
            Some("Choose a poster image smaller than 8 MiB"),
            false,
        )
        .await
        .unwrap_or_else(IntoResponse::into_response),
        Err(error) => error.into_response(),
    })
}
