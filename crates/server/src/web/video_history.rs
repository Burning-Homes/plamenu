//! Complete private viewer settings/history workflows without JavaScript.
use super::{
    layout,
    session::{WebUser, csrf_rejection},
    settings::{field, form_pairs, redirect_to},
};
use crate::{AppState, error::ApiError, video_history as service};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use plamenu_db::video_history as db;
use serde::Deserialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Default, Deserialize)]
pub struct HistoryQuery {
    pub before: Option<String>,
    pub before_media: Option<i64>,
}
impl HistoryQuery {
    pub fn cursor(&self) -> Result<(Option<OffsetDateTime>, i64), ApiError> {
        match (&self.before, self.before_media) {
            (None, None) => Ok((None, 0)),
            (Some(at), Some(id)) if id > 0 => Ok((
                Some(
                    OffsetDateTime::parse(at, &Rfc3339)
                        .map_err(|_| ApiError::BadRequest("Invalid history cursor".into()))?,
                ),
                id,
            )),
            _ => Err(ApiError::BadRequest("Invalid history cursor".into())),
        }
    }
}
pub(crate) fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Cookie, Authorization".parse().unwrap());
    response
}

pub async fn settings(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    let prefs = db::preferences(&state.pool, user.current.account.id).await?;
    let enabled = prefs.history_enabled;
    let l = user.locale;
    let body = html! {section.column.video-viewer-settings.video-form {
        h1 {(l.text("video-viewer-settings"))}
        nav.video-explore__links {a href="/videos/explore" {(l.text("videos-explore-browse"))} a href="/videos/history" {(l.text("video-history-title"))}}
        p {(l.text("video-history-privacy"))}
        form.settings-form method="post" action="/web/videos/settings" {
            input type="hidden" name="csrf" value=(user.csrf);
            label.settings-toggle {input type="checkbox" name="p2p_enabled" value="1" checked[prefs.p2p_enabled] aria-describedby="video-p2p-hint"; span.settings-toggle__label {(l.text("status-p2p-enabled"))}}
            p.settings-field__hint id="video-p2p-hint" {(l.text("status-p2p-explanation")) " " (l.text("video-p2p-settings-hint"))}
            label.settings-toggle {input type="checkbox" name="history_enabled" value="1" checked[enabled] aria-describedby="history-disable-hint"; span.settings-toggle__label {(l.text("video-history-enable"))}}
            p.settings-field__hint id="history-disable-hint" {(l.text("video-history-disable-hint"))}
            button type="submit" {(l.text("video-history-save-settings"))}
        }
    }};
    Ok(private(
        layout::shell(&l.text("video-viewer-settings"), Some(&user), &body).into_response(),
    ))
}
pub async fn settings_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    db::update_preferences(
        &state.pool,
        user.current.account.id,
        Some(field(&pairs, "history_enabled") == Some("1")),
        Some(field(&pairs, "p2p_enabled") == Some("1")),
    )
    .await?;
    Ok(private(redirect_to("/videos/settings")))
}

/// Enhanced player changes only P2P, never history or any other setting.
pub async fn p2p_action(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let enabled = match field(&pairs, "enabled") {
        Some("true") => true,
        Some("false") => false,
        _ => {
            return Err(ApiError::Unprocessable(
                "Choose whether peer sharing is enabled".into(),
            ));
        }
    };
    db::update_preferences(&state.pool, user.current.account.id, None, Some(enabled)).await?;
    Ok(private(
        axum::Json(serde_json::json!({"p2p_enabled":enabled})).into_response(),
    ))
}
pub async fn page(
    State(state): State<AppState>,
    user: WebUser,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, ApiError> {
    let enabled = db::enabled(&state.pool, user.current.account.id).await?;
    let (at, id) = query.cursor()?;
    let (entries, cursor) = service::list(&state, user.current.account.id, at, id).await?;
    let l = user.locale;
    let body = html! {section.column.video-history.video-form {
        h1 {(l.text("video-history-title"))}
        nav.video-explore__links {a href="/videos/explore" {(l.text("videos-explore-browse"))} a href="/videos/settings" {(l.text("video-viewer-settings"))}}
        p {(l.text("video-history-privacy"))}
        @if !enabled {p {a href="/videos/settings" {(l.text("video-history-enable"))}}}
        @if entries.is_empty(){p {(l.text("video-history-empty"))}}
        ul.video-history__entries {@for e in &entries {li {
            h2 {a href=(e["permalink"].as_str().unwrap_or_default()) {(e["title"].as_str().map_or_else(||l.text("status-attachment"),str::to_owned))}}
            p {(l.text("video-history-position")) " " (e["position_seconds"]) " s" @if e["completed"]==true {" · " (l.text("video-history-completed"))}}
            @if let Some(url)=e["resume_url"].as_str(){p {a href=(url) {(l.text("video-history-resume"))}}}
            form method="post" action="/web/videos/history/forget" {
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="media_id" value=(e["media_id"].as_str().unwrap_or_default());
                button type="submit" {(l.text("video-history-forget"))}
            }
        }}}
        @if let Some(cursor)=cursor {
            @let params=serde_urlencoded::to_string([("before",cursor.updated_at.format(&Rfc3339).unwrap_or_default()),("before_media",cursor.media_id.to_string())]).unwrap_or_default();
            a href=(format!("/videos/history?{params}")) {(l.text("video-history-older"))}
        }
        @if enabled {form.settings-form method="post" action="/web/videos/history/forget" {
            input type="hidden" name="csrf" value=(user.csrf);
            label.settings-toggle {input type="checkbox" name="confirm" value="1" required; span {(l.text("video-history-clear-confirm"))}}
            button type="submit" {(l.text("video-history-clear"))}
        }}
    }};
    Ok(private(
        layout::shell(&l.text("video-history-title"), Some(&user), &body).into_response(),
    ))
}
pub async fn forget(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let raw = field(&pairs, "media_id").unwrap_or_default();
    let media = if raw.is_empty() {
        if field(&pairs, "confirm") != Some("1") {
            return Err(ApiError::Unprocessable("Confirm clearing history".into()));
        }
        None
    } else {
        Some(
            raw.parse()
                .map_err(|_| ApiError::Unprocessable("Invalid media id".into()))?,
        )
    };
    db::forget(&state.pool, user.current.account.id, media).await?;
    Ok(private(redirect_to("/videos/history")))
}
pub async fn save(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form_pairs(&body).map_err(ApiError::BadRequest)?;
    if !user.csrf_ok(field(&pairs, "csrf").unwrap_or_default()) {
        return Ok(csrf_rejection());
    }
    let seconds = field(&pairs, "position_seconds")
        .unwrap_or_default()
        .parse()
        .map_err(|_| ApiError::Unprocessable("Use whole seconds".into()))?;
    let revision = field(&pairs, "content_revision")
        .unwrap_or("0")
        .parse()
        .map_err(|_| ApiError::Unprocessable("Reload the film to save a position".into()))?;
    let saved = service::save_revision(
        &state,
        user.current.account.id,
        id,
        seconds,
        field(&pairs, "completed") == Some("1"),
        revision,
    )
    .await?;
    if headers
        .get(header::ACCEPT)
        .is_some_and(|a| a == "application/json")
    {
        return Ok(private(
            axum::Json(serde_json::json!({"saved":saved})).into_response(),
        ));
    }
    if !saved {
        let (_, film) = service::readable(&state, user.current.account.id, id).await?;
        let l = user.locale;
        let body = html! {section.column {
            h1 {(l.text("video-history-title"))}
            p.settings__error role="alert" {(l.text("video-history-revision-failed"))}
            p {a href=(format!("/web/statuses/{}",film.id)) {(l.text("videos-caption-watch"))}}
            p {a href="/videos/settings" {(l.text("video-viewer-settings"))} " · " a href="/videos/history" {(l.text("video-history-title"))}}
        }};
        return Ok(private(
            (
                axum::http::StatusCode::CONFLICT,
                layout::shell(&l.text("video-history-title"), Some(&user), &body),
            )
                .into_response(),
        ));
    }
    Ok(private(redirect_to("/videos/history")))
}
