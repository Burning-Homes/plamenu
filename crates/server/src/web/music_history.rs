//! Private native history and listening preferences use ordinary CSRF forms.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_listening::{self, PreferenceChange, Progress, Start},
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use maud::html;
use serde::Deserialize;
#[derive(Default, Deserialize)]
pub struct Page {
    max_id: Option<i64>,
}
pub async fn index(
    State(state): State<AppState>,
    user: WebUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    let prefs = music_listening::preferences(&state, user.current.account.id).await?;
    let history = music_listening::history(&state, user.current.account.id, page.max_id).await?;
    let l = user.locale;
    let clock = super::pages::clock_for(Some(&user), l);
    let body = html! {section.column.music-history {
        h1{(l.text("music-history-title"))}
        (super::music::workspace_navigation(l,true,"history"))
        p{(l.text("music-history-hint"))}
        p{a href="/music"{(l.text("music-title"))} " · " a href="/music/stats"{(l.text("music-stats-title"))} " · " a.music-player-open href="/music/player" target="plamenu-music-player"{(l.text("music-player-title"))}}
        details{summary{(l.text("music-history-preferences"))}
            form.settings-form method="post" action="/web/music/history/preferences" {
                input type="hidden" name="csrf" value=(user.csrf);input type="hidden" name="revision" value=(prefs.revision);
                label{input type="checkbox" name="history_enabled" value="true" checked[prefs.history_enabled];(l.text("music-history-enabled"))}
                label for="music-history-seconds"{(l.text("music-history-seconds"))}
                input id="music-history-seconds" name="minimum_seconds" type="number" min="1" max="600" value=(prefs.minimum_seconds) required;
                label for="music-history-percent"{(l.text("music-history-percent"))}
                input id="music-history-percent" name="minimum_percent" type="number" min="1" max="100" value=(prefs.minimum_percent) required;
                p{(l.text("music-history-threshold-hint"))}
                button type="submit"{(l.text("music-history-save"))}
            }
        }
        @if history.items.is_empty(){p{(l.text("music-history-empty"))}}
        ol{@for item in &history.items {li {
            a href=(item.track.url){(item.track.title)} " — " (item.track.artist)
            p{(clock.element_absolute_iso(&item.counted_at))}
        }}}
        @if let Some(before)=&history.before{a href=(format!("/music/history?max_id={before}")){(l.text("music-older"))}}
        details{summary{(l.text("music-history-erase"))}
            p{(l.text("music-history-erase-hint"))}
            form.settings-form method="post" action="/web/music/history/erase"{
                input type="hidden" name="csrf" value=(user.csrf);
                label{input type="checkbox" name="confirm" value="true" required;(l.text("music-history-erase-confirm"))}
                button type="submit"{(l.text("music-history-erase"))}
            }
        }
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&l.text("music-history-title"), Some(&user), &body),
    )
        .into_response())
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    user.current.require_scope("write:statuses")?;
    Ok(pairs)
}
pub async fn preferences(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let field = |key| settings::field(&pairs, key).unwrap_or_default();
    let number = |key| {
        field(key)
            .parse::<i32>()
            .map_err(|_| ApiError::Unprocessable("Invalid listening threshold".into()))
    };
    music_listening::set_preferences(
        &state,
        user.current.account.id,
        &PreferenceChange {
            revision: field("revision").into(),
            history_enabled: field("history_enabled") == "true",
            minimum_seconds: number("minimum_seconds")?,
            minimum_percent: number("minimum_percent")?,
        },
    )
    .await?;
    Ok(settings::redirect_to("/music/history"))
}
pub async fn erase(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    if settings::field(&pairs, "confirm") != Some("true") {
        return Err(ApiError::Unprocessable("Confirm history erasure".into()));
    }
    music_listening::erase(&state, user.current.account.id).await?;
    Ok(settings::redirect_to("/music/history"))
}
pub async fn start(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let input = Start {
        remote_id: settings::field(&pairs, "remote_id").map(str::to_owned),
        episode_id: settings::field(&pairs, "episode_id").map(str::to_owned),
        request_key: settings::field(&pairs, "request_key")
            .unwrap_or_default()
            .into(),
        asset_id: settings::field(&pairs, "asset_id").map(str::to_owned),
        status_id: settings::field(&pairs, "status_id").map(str::to_owned),
        queue_device: settings::field(&pairs, "queue_device").map(str::to_owned),
        queue_entry_id: settings::field(&pairs, "queue_entry_id").map(str::to_owned),
    };
    let session = music_listening::start(&state, user.current.account.id, &input).await?;
    if headers
        .get(header::ACCEPT)
        .is_some_and(|v| v == "application/json")
    {
        return Ok((
            [(header::CACHE_CONTROL, "private, no-store")],
            Json(session),
        )
            .into_response());
    }
    Ok(settings::redirect_to("/music/history"))
}
pub async fn progress(
    State(state): State<AppState>,
    user: WebUser,
    Path(token): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let seconds = settings::field(&pairs, "played_seconds")
        .unwrap_or_default()
        .parse::<f64>()
        .map_err(|_| ApiError::Unprocessable("Invalid played time".into()))?;
    let session = music_listening::progress(
        &state,
        user.current.account.id,
        &token,
        &Progress {
            played_seconds: seconds,
            finish: settings::field(&pairs, "finish") == Some("true"),
        },
    )
    .await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(session),
    )
        .into_response())
}
