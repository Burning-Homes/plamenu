//! Owner-only album access controls use native CSRF forms.
use super::{layout, session::WebUser, settings};
use crate::{
    AppState,
    error::ApiError,
    music_album_access,
    music_entities::{self, Kind},
};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use maud::html;
#[derive(Default, serde::Deserialize)]
pub struct Page {
    max_id: Option<i64>,
}
pub async fn show(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    user.current.require_scope("read:statuses")?;
    let album = music_entities::get(&state, Kind::Album, id, Some(user.current.account.id)).await?;
    let members =
        music_album_access::members(&state, user.current.account.id, id, page.max_id).await?;
    let l = user.locale;
    let title = l.text("music-album-access-title");
    let body = html! {section.column.music-library {
        h1{(title)}h2{(album.name)}
        p{(l.text("music-album-access-hint"))}
        form.settings-form method="post" action=(format!("/web/music/albums/{id}/access")){
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="revision" value=(album.revision);
            label for="album-visibility"{(l.text("music-album-access-details"))}
            select id="album-visibility" name="visibility"{
                @for (value,key) in [("private","music-assets-private"),("local","music-assets-local"),("public","visibility-public"),("source","music-album-access-source")] {
                    option value=(value) selected[value==album.visibility]{(l.text(key))}
                }
            }
            label{input type="checkbox" name="share_tracks" value="true" checked[album.share_tracks];" "(l.text("music-collection-share-tracks"))}
            p{(l.text("music-album-access-share-hint"))}
            button type="submit"{(l.text("music-album-access-save"))}
        }
        h2{(l.text("music-assets-share"))}
        form.settings-form method="post" action=(format!("/web/music/albums/{id}/grant")){
            input type="hidden" name="csrf" value=(user.csrf);
            input type="hidden" name="operation" value="grant";
            label for="album-member"{(l.text("music-assets-member"))}
            input id="album-member" name="member" required maxlength="64";
            button type="submit"{(l.text("music-assets-grant"))}
        }
        @if members.items.is_empty(){p{(l.text("music-album-access-members-empty"))}}
        ul{@for member in members.items {li{
            (member.username)
            form method="post" action=(format!("/web/music/albums/{id}/grant")){
                input type="hidden" name="csrf" value=(user.csrf);
                input type="hidden" name="operation" value="revoke";
                input type="hidden" name="member" value=(member.username);
                button type="submit"{(l.text("music-assets-revoke"))}
            }
        }}}
        @if let Some(before)=members.before{a href=(format!("/music/albums/{id}/access?max_id={before}")){(l.text("music-older"))}}
        p{a href=(format!("/music/albums/{id}")){(album.name)}}
    }};
    Ok((
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        layout::shell(&title, Some(&user), &body),
    )
        .into_response())
}
fn form(user: &WebUser, body: &Bytes) -> Result<Vec<(String, String)>, ApiError> {
    user.current.require_scope("write:statuses")?;
    let pairs = settings::form_pairs(body).map_err(ApiError::Unprocessable)?;
    if settings::field(&pairs, "csrf") != Some(user.csrf.as_str()) {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    Ok(pairs)
}
pub async fn update(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    music_album_access::update(
        &state,
        user.current.account.id,
        id,
        &music_album_access::Edit {
            revision: settings::field(&pairs, "revision")
                .unwrap_or_default()
                .to_owned(),
            visibility: settings::field(&pairs, "visibility")
                .unwrap_or_default()
                .to_owned(),
            share_tracks: settings::field(&pairs, "share_tracks") == Some("true"),
        },
    )
    .await?;
    Ok(settings::redirect_to(&format!("/music/albums/{id}/access")))
}
pub async fn grant(
    State(state): State<AppState>,
    user: WebUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let pairs = form(&user, &body)?;
    let member = plamenu_db::account::find_local_by_username(
        &state.pool,
        settings::field(&pairs, "member").unwrap_or_default(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let enabled = match settings::field(&pairs, "operation") {
        Some("grant") => true,
        Some("revoke") => false,
        _ => return Err(ApiError::Unprocessable("Choose grant or revoke".into())),
    };
    music_album_access::grant(&state, user.current.account.id, id, member.id, enabled).await?;
    Ok(settings::redirect_to(&format!("/music/albums/{id}/access")))
}
