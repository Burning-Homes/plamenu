//! HTML-first, private channel delivery analytics and collection controls.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError, video_analytics as service};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie".parse().unwrap());
    response
}
pub async fn page(State(state): State<AppState>, user: WebUser, Path(id): Path<i64>) -> Response {
    let result = async {
        user.current.require_scope("read:media")?;
        let role = service::access(&state, user.current.account.id, id).await?;
        let policy = service::policy(&state, id).await?;
        let daily = service::daily(&state, id).await?;
        let locale = user.locale;
        let body = html! {
            section.column.video-analytics.video-form {
                h1 { (locale.text("video-analytics-title")) }
                p { a href=(format!("/videos/channels/{id}")) { (locale.text("video-channel-manage")) } }
                p { (locale.text("video-analytics-hint")) }
                p { (locale.text("video-analytics-limit-hint")) }
                @if !state.config.video_analytics { p role="status" { (locale.text("video-analytics-operator-disabled")) } }
                @if role == "owner" {
                    form.settings-form method="post" action=(format!("/web/video-channels/{id}/analytics")) {
                        input type="hidden" name="csrf" value=(user.csrf);
                        label.settings-field__choice {
                            input type="checkbox" name="enabled" value="yes" checked[policy.enabled];
                            span { (locale.text("video-analytics-enable")) }
                        }
                        label.settings-field {
                            span.settings-field__label { (locale.text("video-analytics-retention")) }
                            input type="number" name="retention_days" min="1" max="90" required value=(policy.retention_days);
                        }
                        button type="submit" name="operation" value="save" { (locale.text("video-analytics-save")) }
                        label.settings-field__choice {
                            input type="checkbox" name="confirm_erase" value="yes";
                            span { (locale.text("video-analytics-confirm")) }
                        }
                        button.settings-button--danger type="submit" name="operation" value="erase" formnovalidate { (locale.text("video-analytics-erase")) }
                    }
                }
                @if daily.is_empty() { p { (locale.text("video-analytics-empty")) } }
                @else {
                    ol.video-analytics__days {
                        @for row in &daily {
                            li {
                                dl {
                                    dt { (locale.text("video-analytics-day")) } dd { (row.day) }
                                    dt { (locale.text("video-analytics-video")) }
                                    dd { a href=(format!("/web/statuses/{}",row.status_id)) { (row.title) } }
                                    dt { (locale.text("video-analytics-lane")) } dd { (locale.text(&format!("video-analytics-lane-{}",row.lane))) }
                                    dt { (locale.text("video-analytics-requests")) } dd { (row.requests) }
                                    dt { (locale.text("video-analytics-bytes")) } dd { (row.body_bytes) }
                                }
                            }
                        }
                    }
                }
            }
        };
        Ok::<_,ApiError>(layout::shell(&locale.text("video-analytics-title"),Some(&user),&body).into_response())
    }.await;
    private(result.unwrap_or_else(IntoResponse::into_response))
}
pub async fn save(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Response {
    let result = async {
        let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
        if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
            return Ok(csrf_rejection());
        }
        user.current.require_scope("write:media")?;
        let erase = field(&pairs, "operation") == Some("erase");
        if erase && field(&pairs, "confirm_erase") != Some("yes") {
            return Err(ApiError::Unprocessable(
                user.locale.text("video-analytics-confirm"),
            ));
        }
        let retention = if erase {
            30
        } else {
            field(&pairs, "retention_days")
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| {
                    ApiError::Unprocessable(user.locale.text("video-analytics-retention"))
                })?
        };
        // Erasure is deliberately independent of unsaved collection choices.
        service::change(
            &state,
            user.current.account.id,
            id,
            field(&pairs, "enabled") == Some("yes"),
            retention,
            erase,
        )
        .await?;
        Ok::<_, ApiError>(redirect_to(&format!("/videos/channels/{id}/analytics")))
    }
    .await;
    private(result.unwrap_or_else(IntoResponse::into_response))
}
