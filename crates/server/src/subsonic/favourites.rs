//! Protocol stars are the native account's private Music favourites.
use super::{
    playlists,
    protocol::{Error, Params},
};
use crate::{AppState, error::ApiError, music_clients::auth::Principal, music_favourites};
use serde_json::{Value, json};
pub async fn dispatch(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Value, Error> {
    if matches!(method, "star" | "unstar") {
        principal.require_manage()?;
        let ids = params.all("id");
        if ids.len() > 500 {
            return Err(ApiError::Unprocessable("Choose up to 500 songs".into()).into());
        }
        for id in ids {
            let Ok(source) = playlists::source(id) else {
                continue;
            };
            principal.current(state).await?;
            let result = if method == "star" {
                music_favourites::save(state, principal.owner, &source)
                    .await
                    .map(|_| ())
            } else {
                music_favourites::remove(state, principal.owner, &source).await
            };
            match result {
                Ok(()) | Err(ApiError::NotFound) => {}
                Err(error) => return Err(error.into()),
            }
        }
        return Ok(json!({}));
    }
    let mut before = None;
    let mut songs = Vec::new();
    loop {
        let page = music_favourites::page(state, principal.owner, before).await?;
        for item in page.items {
            let entry = crate::music_collections::Entry {
                id: 1,
                collection_id: 0,
                position: 1,
                asset_id: item
                    .asset_id
                    .as_deref()
                    .map(super::catalogue::id)
                    .transpose()?,
                status_id: item
                    .status_id
                    .as_deref()
                    .map(super::catalogue::id)
                    .transpose()?,
                remote_id: item
                    .remote_id
                    .as_deref()
                    .map(super::catalogue::id)
                    .transpose()?,
                episode_id: None,
            };
            match playlists::song(state, principal.owner, &entry).await {
                Ok(mut song) => {
                    song["starred"] = json!(item.created_at);
                    songs.push(song);
                }
                Err(error) if error.code == 70 => {}
                Err(error) => return Err(error),
            }
        }
        before = page
            .before
            .as_deref()
            .map(super::catalogue::id)
            .transpose()?;
        if before.is_none() {
            break;
        }
    }
    let key = if method == "getStarred" {
        "starred"
    } else {
        "starred2"
    };
    Ok(json!({key:{"artist":[],"album":[],"song":songs}}))
}
