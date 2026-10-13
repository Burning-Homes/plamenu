//! One-time native credential creation and revocation inside Music.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_clients::{self, Issued, Request},
};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
async fn page(
    state: &AppState,
    user: &WebUser,
    issued: Option<&Issued>,
) -> Result<Response, ApiError> {
    let keys = music_clients::list(state, user.current.account.id).await?;
    let l = user.locale;
    let body = html! {section.column.music-library{
        h1{(l.text("music-clients-title"))}
        (super::music::workspace_navigation(l,true,"clients"))
        p{(l.text("music-clients-hint"))}
        p{(l.text("music-clients-server"))" "code{(format!("https://{}",state.config.domain))}}
        p{(l.text("music-clients-user"))" "code{(user.current.account.username)}}
        @if let Some(issued)=issued {section.music-client-issued{
            h2{(l.text("music-clients-created"))}p{(l.text("music-clients-once"))}
            label for="music-client-password"{(l.text("music-clients-password"))}
            input id="music-client-password" type="text" readonly value=(issued.password) autocomplete="off" spellcheck="false";
        }}
        @if user.current.has_scope("write:accounts") && user.current.has_scope("read:statuses"){
            form.settings-form method="post" action="/web/music/clients/create"{
                input type="hidden" name="csrf" value=(user.csrf);
                label for="music-client-label"{(l.text("music-clients-label"))}input id="music-client-label" type="text" name="label" required maxlength="100";
                label for="music-client-days"{(l.text("music-clients-days"))}input id="music-client-days" type="number" name="days" min="1" max="365" value="90" required;
                @if user.current.has_scope("write:statuses"){label{input type="checkbox" name="can_manage" value="true";(l.text("music-clients-manage"))}label{input type="checkbox" name="broadcast_playing" value="true";(l.text("music-clients-broadcast"))}}
                button type="submit"{(l.text("music-clients-create"))}
            }
        }
        ul{@for key in keys {li{
            strong{(key.label)}" · "(l.text(if key.can_manage{"music-clients-manage"}else{"music-clients-read"}))
            @if key.broadcast_playing{ " · "(l.text("music-clients-broadcast"))}
            " · "(l.text("music-clients-expires"))" " time datetime=(key.expires_at.to_string()){(key.expires_at.to_string())}
            @if user.current.has_scope("write:accounts"){form method="post" action=(format!("/web/music/clients/{}/revoke",key.id)){
                input type="hidden" name="csrf" value=(user.csrf);button type="submit"{(l.text("music-clients-revoke"))}
            }}
        }}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        layout::shell(&l.text("music-clients-title"), Some(user), &body),
    )
        .into_response())
}
pub async fn index(State(state): State<AppState>, user: WebUser) -> Result<Response, ApiError> {
    user.current.require_scope("read:accounts")?;
    page(&state, &user, None).await
}
pub async fn create(
    State(state): State<AppState>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:accounts")?;
    user.current.require_scope("read:statuses")?;
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    let input = Request {
        label: settings::field(&pairs, "label").unwrap_or_default().into(),
        days: settings::field(&pairs, "days")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        can_manage: settings::field(&pairs, "can_manage") == Some("true"),
        broadcast_playing: settings::field(&pairs, "broadcast_playing") == Some("true"),
    };
    if input.can_manage {
        user.current.require_scope("write:statuses")?;
    }
    // A first-party client credential survives a browser logout. Keys minted by
    // the API retain their originating OAuth grant and its current scopes.
    let issued = music_clients::create(&state, user.current.account.id, None, &input).await?;
    page(&state, &user, Some(&issued)).await
}
pub async fn revoke(
    State(state): State<AppState>,
    Path(key): Path<i64>,
    user: WebUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.current.require_scope("write:accounts")?;
    let pairs = settings::form_pairs(&body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    music_clients::revoke(&state, user.current.account.id, key).await?;
    Ok(settings::redirect_to("/music/clients"))
}
