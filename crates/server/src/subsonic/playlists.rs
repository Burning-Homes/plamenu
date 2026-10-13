//! Protocol playlist indexes refer to the same visible entries returned to clients.
use super::{
    catalogue,
    protocol::{Error, Params},
};
use crate::{
    AppState,
    error::ApiError,
    music_clients::auth::Principal,
    music_collections::{self, Collection, Entry},
    music_queues::Source,
};

use serde_json::{Value, json};
use std::collections::HashSet;

pub(super) fn source(value: &str) -> Result<Source, Error> {
    let (kind, id) = value
        .split_once('-')
        .map_or(("asset", value), |(kind, id)| (kind, id));
    let id = catalogue::id(id)?.to_string();
    let mut source = Source {
        asset_id: None,
        status_id: None,
        episode_id: None,
        remote_id: None,
    };
    match kind {
        "asset" => source.asset_id = Some(id),
        "status" => source.status_id = Some(id),
        "remote" => source.remote_id = Some(id),
        _ => return Err(ApiError::NotFound.into()),
    }
    Ok(source)
}
pub async fn song_entries(
    state: &AppState,
    owner: i64,
    row: &Collection,
) -> Result<Vec<(Entry, Value)>, Error> {
    let rows = music_collections::entries(state, row.id).await?;
    songs(state, owner, &rows).await
}
/// Batch current visibility and catalogue fields for any reported entry set.
pub(super) async fn songs(
    state: &AppState,
    owner: i64,
    rows: &[Entry],
) -> Result<Vec<(Entry, Value)>, Error> {
    let mut result = Vec::new();
    for page in rows.chunks(100) {
        let rows = crate::music_remote::readable_entries(&state.pool, page, Some(owner)).await?;
        let playables = music_collections::sources(state, &rows, Some(owner))
            .await?
            .into_iter()
            .map(|row| (row.entry_id.clone(), row))
            .collect::<std::collections::HashMap<_, _>>();
        let assets = crate::music_assets::for_ids(
            state,
            &rows
                .iter()
                .filter_map(|row| row.asset_id)
                .collect::<Vec<_>>(),
            Some(owner),
        )
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect::<std::collections::HashMap<_, _>>();
        for entry in rows {
            if entry.episode_id.is_some() {
                continue;
            }
            let Some(playable) = playables.get(&entry.id.to_string()) else {
                continue;
            };
            let song = if let Some(asset) = entry.asset_id.and_then(|id| assets.get(&id)) {
                catalogue::song(asset)
            } else {
                let source = if let Some(remote) = entry.remote_id {
                    format!("remote-{remote}")
                } else if let Some(status) = entry.status_id {
                    format!("status-{status}")
                } else {
                    continue;
                };
                json!({"id":source,"title":playable.title,"artist":playable.artist,"duration":catalogue::rounded_seconds(playable.duration),"isDir":false,"type":"music","isVideo":false})
            };
            result.push((entry, song));
        }
    }
    Ok(result)
}
async fn entity(
    state: &AppState,
    owner: i64,
    row: &Collection,
    entries: bool,
) -> Result<Value, Error> {
    let songs = song_entries(state, owner, row).await?;
    let owner = plamenu_db::account::find_by_id(&state.pool, row.owner_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let duration = songs
        .iter()
        .filter_map(|(_, song)| song["duration"].as_f64())
        .sum::<f64>();
    let mut value = json!({"id":row.id.to_string(),"name":row.name,"comment":row.description,"owner":owner.username,"public":row.visibility=="public","songCount":songs.len(),"duration":catalogue::rounded_seconds(duration),"created":crate::entities::rfc3339(row.updated_at).ok(),"changed":crate::entities::rfc3339(row.updated_at).ok()});
    if entries {
        value["entry"] = json!(songs.into_iter().map(|(_, song)| song).collect::<Vec<_>>());
    }
    Ok(value)
}
async fn require(state: &AppState, owner: i64, params: &Params) -> Result<Collection, Error> {
    let id = catalogue::id(
        params
            .get("id")
            .or_else(|| params.get("playlistId"))
            .ok_or_else(|| Error {
                code: 10,
                message: "Required parameter is missing: id".into(),
            })?,
    )?;
    let row = music_collections::get(state, id, Some(owner)).await?;
    if row.kind != "playlist" {
        return Err(ApiError::NotFound.into());
    }
    Ok(row)
}
pub async fn dispatch(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Value, Error> {
    match method {
        "getPlaylists" => {
            let username = params.get("username");
            let mut playlists = Vec::new();
            for row in catalogue::collections(state, principal.owner, "playlist").await? {
                let value = entity(state, principal.owner, &row, false).await?;
                if username.is_none_or(|name| value["owner"].as_str() == Some(name)) {
                    playlists.push(value);
                }
            }
            Ok(json!({"playlists":{"playlist":playlists}}))
        }
        "getPlaylist" => {
            let row = require(state, principal.owner, params).await?;
            Ok(json!({"playlist":entity(state,principal.owner,&row,true).await?}))
        }
        "deletePlaylist" => {
            principal.require_manage()?;
            let row = require(state, principal.owner, params).await?;
            music_collections::delete(state, principal.owner, row.id, row.revision).await?;
            Ok(json!({}))
        }
        "createPlaylist" | "updatePlaylist" => {
            principal.require_manage()?;
            let row = edit(state, principal, method, params).await?;
            if method == "createPlaylist" {
                Ok(json!({"playlist":entity(state,principal.owner,&row,true).await?}))
            } else {
                Ok(json!({}))
            }
        }
        _ => Err(Error {
            code: 0,
            message: "Unknown playlist method".into(),
        }),
    }
}
struct Change {
    previous: Option<Collection>,
    input: music_collections::New,
    replace: bool,
    remove: HashSet<i64>,
    sources: Vec<Entry>,
}
async fn change(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Change, Error> {
    let previous = if method == "createPlaylist" {
        params.get("playlistId").map(catalogue::id).transpose()?
    } else {
        Some(catalogue::id(
            params
                .get("playlistId")
                .or_else(|| params.get("id"))
                .ok_or_else(|| Error {
                    code: 10,
                    message: "Required parameter is missing: playlistId".into(),
                })?,
        )?)
    };
    let previous = if let Some(id) = previous {
        Some(music_collections::get(state, id, Some(principal.owner)).await?)
    } else {
        None
    };
    if previous
        .as_ref()
        .is_some_and(|row| row.owner_id != principal.owner || row.kind != "playlist")
    {
        return Err(ApiError::NotFound.into());
    }
    let input = music_collections::New {
        name: params
            .get("name")
            .map(str::to_owned)
            .or_else(|| previous.as_ref().map(|row| row.name.clone()))
            .ok_or_else(|| Error {
                code: 10,
                message: "Required parameter is missing: name".into(),
            })?,
        description: params
            .get("comment")
            .map(str::to_owned)
            .or_else(|| previous.as_ref().map(|row| row.description.clone()))
            .unwrap_or_default(),
        kind: "playlist".into(),
        visibility: if params.get("public").is_some() {
            if params.boolean("public", false) {
                "public"
            } else {
                "private"
            }
            .into()
        } else {
            previous
                .as_ref()
                .map_or("private", |row| row.visibility.as_str())
                .into()
        },
    };
    let raw = params.all(if method == "createPlaylist" {
        "songId"
    } else {
        "songIdToAdd"
    });
    if raw.len() > 500 {
        return Err(ApiError::Unprocessable("Add up to 500 songs per request".into()).into());
    }
    let replace = method == "createPlaylist" && !raw.is_empty();
    let sources = raw
        .into_iter()
        .filter_map(|value| source(value).ok())
        .enumerate()
        .map(|(index, source)| source.entry(i64::try_from(index + 1).unwrap_or(501), 0, 1))
        .collect::<Result<Vec<_>, _>>()?;
    let remove = if let Some(row) = &previous {
        let songs = song_entries(state, principal.owner, row).await?;
        let indices = params
            .all("songIndexToRemove")
            .into_iter()
            .filter_map(|value| value.parse::<usize>().ok())
            .collect::<HashSet<_>>();
        songs
            .into_iter()
            .enumerate()
            .filter(|(index, _)| replace || indices.contains(index))
            .map(|(_, (entry, _))| entry.id)
            .collect()
    } else {
        HashSet::new()
    };
    Ok(Change {
        previous,
        input,
        replace,
        remove,
        sources,
    })
}
async fn edit(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Collection, Error> {
    let change = change(state, principal, method, params).await?;
    // A single owner/collection transaction preserves podcast occurrences and
    // resolves concurrent native edits through the same revision fence.
    Ok(music_collections::edit_client_playlist(
        state,
        principal,
        &change.input,
        change.previous.as_ref(),
        &change.remove,
        &change.sources,
        change.replace,
    )
    .await?)
}

pub(super) async fn song(state: &AppState, owner: i64, entry: &Entry) -> Result<Value, Error> {
    if let Some(asset) = entry.asset_id {
        return Ok(catalogue::song(
            &crate::music_assets::get(state, asset, Some(owner)).await?,
        ));
    }
    let playable = music_collections::sources(state, std::slice::from_ref(entry), Some(owner))
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    let source = if let Some(remote) = entry.remote_id {
        format!("remote-{remote}")
    } else if let Some(status) = entry.status_id {
        format!("status-{status}")
    } else {
        return Err(ApiError::NotFound.into());
    };
    Ok(
        json!({"id":source,"title":playable.title,"artist":playable.artist,"duration":catalogue::rounded_seconds(playable.duration),"isDir":false,"isVideo":false,"type":"music"}),
    )
}
