//! Catalogue methods translate current Music resources, never storage paths.
use super::protocol::{Error, Params};
use crate::{
    AppState,
    error::ApiError,
    music_assets::{self, Asset},
    music_collections::{self, Collection},
    music_entities::{self, Entity, Kind},
};
use plamenu_db::DbError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use time::OffsetDateTime;

pub fn id(value: &str) -> Result<i64, Error> {
    value
        .parse()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::NotFound.into())
}
fn compact(mut value: Value) -> Value {
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|_, item| !item.is_null());
    }
    value
}
pub(super) fn rounded_seconds(seconds: f64) -> i64 {
    if seconds.is_finite() {
        seconds
            .clamp(0.0, 2_147_483_647.0)
            .floor()
            .to_string()
            .parse()
            .unwrap_or(0)
    } else {
        0
    }
}
pub fn song(asset: &Asset) -> Value {
    compact(
        json!({"id":asset.id.to_string(),"parent":asset.album_id.map(|id|id.to_string()),"isDir":false,"title":asset.title,"album":asset.album,"artist":asset.artist,"track":asset.track_number,"discNumber":asset.disc_number,"year":asset.release_year,"genre":asset.genre,"coverArt":asset.artwork_id.map(|_|format!("tr-{}",asset.id)),"size":asset.original_bytes,"contentType":mime(&asset.original_format),"suffix":asset.original_format,"duration":rounded_seconds(asset.duration),"path":format!("{}.{}",asset.title.replace(['/', '\\'],"_"),asset.original_format),"albumId":asset.album_id.map(|id|id.to_string()),"artistId":asset.artist_id.to_string(),"type":"music","isVideo":false}),
    )
}
pub fn mime(format: &str) -> &'static str {
    match format {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "opus" => "audio/ogg; codecs=opus",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        "aiff" => "audio/aiff",
        _ => "application/octet-stream",
    }
}
fn artist(row: &Entity, albums: &[Entity]) -> Value {
    compact(
        json!({"id":row.id.to_string(),"name":row.name,"albumCount":albums.iter().filter(|album|album.artist_id==Some(row.id)).count()}),
    )
}
fn album(row: &Entity, tracks: &[Asset], directory: bool) -> Value {
    let tracks = tracks
        .iter()
        .filter(|track| track.album_id == Some(row.id))
        .collect::<Vec<_>>();
    let mut value = compact(
        json!({"id":row.id.to_string(),"name":row.name,"artist":row.artist,"artistId":row.artist_id.map(|id|id.to_string()),"songCount":tracks.len(),"duration":rounded_seconds(tracks.iter().map(|track|track.duration).sum()),"year":row.release_year,"genre":row.genres.first(),"created":crate::entities::rfc3339(row.created_at).ok()}),
    );
    if directory {
        value["isDir"] = json!(true);
        value["title"] = json!(row.name);
        value["parent"] = json!(row.artist_id.map(|id| id.to_string()));
    }
    value
}
pub async fn collections(
    state: &AppState,
    owner: i64,
    kind: &str,
) -> Result<Vec<Collection>, Error> {
    let mut result = Vec::new();
    let mut after = None;
    loop {
        let ids:Vec<i64>=sqlx::query_scalar("SELECT c.id FROM music_collections c WHERE c.kind=$3 AND ($2::bigint IS NULL OR c.id<$2) AND (c.owner_id=$1 OR c.visibility IN('public','local') OR EXISTS(SELECT 1 FROM music_collection_grants g WHERE g.collection_id=c.id AND g.account_id=$1)) ORDER BY c.id DESC LIMIT 100")
            .bind(owner).bind(after).bind(kind).fetch_all(&state.pool).await.map_err(DbError::from).map_err(ApiError::from)?;
        result.extend(music_collections::for_ids(state, &ids, Some(owner)).await?);
        if ids.len() < 100 || result.len() >= 500 {
            result.truncate(500);
            return Ok(result);
        }
        after = ids.last().copied();
    }
}
struct Catalogue {
    tracks: Vec<Asset>,
    artists: Vec<Entity>,
    albums: Vec<Entity>,
}
impl Catalogue {
    async fn load(state: &AppState, owner: i64, params: &Params) -> Result<Self, Error> {
        let library = params
            .get("musicFolderId")
            .filter(|value| !matches!(*value, "0" | "-1"))
            .map(id)
            .transpose()?;
        if let Some(library) = library {
            let row = music_collections::get(state, library, Some(owner)).await?;
            if row.kind != "library" {
                return Err(ApiError::NotFound.into());
            }
        }
        let mut tracks = Vec::new();
        let mut before = None;
        loop {
            let rows:Vec<Asset>=sqlx::query_as(music_assets::asset_query!(" WHERE ($1::bigint IS NULL OR t.id<$1) "," AND ($3::bigint IS NULL OR EXISTS(SELECT 1 FROM music_collection_entries e WHERE e.collection_id=$3 AND (e.asset_id=t.id OR EXISTS(SELECT 1 FROM music_publications p WHERE p.asset_id=t.id AND p.status_id=e.status_id)))) ORDER BY t.id DESC LIMIT 100"))
                .bind(before).bind(Some(owner)).bind(library).fetch_all(&state.pool).await.map_err(DbError::from).map_err(ApiError::from)?;
            before = rows.last().map(|row| row.id);
            let last = rows.len() < 100;
            tracks.extend(rows);
            if last || tracks.len() >= 10000 {
                break;
            }
        }
        let mut artist_ids = tracks
            .iter()
            .flat_map(|track| [Some(track.artist_id), track.album_artist_id])
            .flatten()
            .collect::<HashSet<_>>();
        let mut album_ids = tracks
            .iter()
            .filter_map(|track| track.album_id)
            .collect::<HashSet<_>>();
        if library.is_none() {
            let owned_artists: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM music_artists WHERE owner_id=$1 ORDER BY id DESC LIMIT 10000",
            )
            .bind(owner)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)
            .map_err(ApiError::from)?;
            let owned_albums: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM music_albums WHERE owner_id=$1 ORDER BY id DESC LIMIT 10000",
            )
            .bind(owner)
            .fetch_all(&state.pool)
            .await
            .map_err(DbError::from)
            .map_err(ApiError::from)?;
            artist_ids.extend(owned_artists);
            album_ids.extend(owned_albums);
        }
        let albums = entities(state, owner, Kind::Album, album_ids).await?;
        artist_ids.extend(albums.iter().filter_map(|row| row.artist_id));
        let artists = entities(state, owner, Kind::Artist, artist_ids).await?;
        tracks.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        Ok(Self {
            tracks,
            artists,
            albums,
        })
    }
    fn album(&self, id: i64) -> Result<&Entity, Error> {
        self.albums
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| ApiError::NotFound.into())
    }
    fn artist(&self, id: i64) -> Result<&Entity, Error> {
        self.artists
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| ApiError::NotFound.into())
    }
    fn songs(&self, params: &Params, genre: Option<&str>) -> Vec<Value> {
        let from = params
            .get("fromYear")
            .and_then(|value| value.parse::<i32>().ok());
        let to = params
            .get("toYear")
            .and_then(|value| value.parse::<i32>().ok());
        self.tracks
            .iter()
            .filter(|track| {
                genre.is_none_or(|genre| track.genre.eq_ignore_ascii_case(genre))
                    && from.is_none_or(|from| track.release_year.is_some_and(|year| year >= from))
                    && to.is_none_or(|to| track.release_year.is_some_and(|year| year <= to))
            })
            .skip(params.number("offset", 0, 10000))
            .take(params.number("count", params.number("size", 10, 500), 500))
            .map(song)
            .collect()
    }
}
async fn entities(
    state: &AppState,
    owner: i64,
    kind: Kind,
    ids: HashSet<i64>,
) -> Result<Vec<Entity>, Error> {
    let mut result = Vec::new();
    let ids = ids.into_iter().collect::<Vec<_>>();
    for page in ids.chunks(100) {
        result.extend(music_entities::for_ids(state, kind, page, Some(owner)).await?);
    }
    result.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    Ok(result)
}
fn index(catalogue: &Catalogue) -> Vec<Value> {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in &catalogue.artists {
        let name = row
            .name
            .split_once(' ')
            .filter(|(article, _)| {
                matches!(
                    article.to_ascii_lowercase().as_str(),
                    "the" | "a" | "an" | "el" | "la" | "le" | "les"
                )
            })
            .map_or(row.name.as_str(), |(_, rest)| rest);
        let letter = name
            .chars()
            .next()
            .filter(|ch| ch.is_alphabetic())
            .map_or_else(|| "#".into(), |ch| ch.to_uppercase().collect::<String>());
        groups
            .entry(letter)
            .or_default()
            .push(artist(row, &catalogue.albums));
    }
    groups
        .into_iter()
        .map(|(name, artist)| json!({"name":name,"artist":artist}))
        .collect()
}
pub async fn dispatch(
    state: &AppState,
    owner: i64,
    method: &str,
    params: &Params,
) -> Result<Value, Error> {
    if method == "getSong" {
        let source = super::playlists::source(params.require("id")?)?;
        let mut entry = source.entry(1, 0, 1)?;
        crate::music_remote::normalize_sources(state, std::slice::from_mut(&mut entry), owner)
            .await?;
        return Ok(json!({"song":super::playlists::song(state,owner,&entry).await?}));
    }
    if method == "getMusicFolders" {
        let mut folders = vec![json!({"id":"0","name":"Plamenu"})];
        folders.extend(
            collections(state, owner, "library")
                .await?
                .into_iter()
                .map(|row| json!({"id":row.id.to_string(),"name":row.name})),
        );
        return Ok(json!({"musicFolders":{"musicFolder":folders}}));
    }
    let catalogue = Catalogue::load(state, owner, params).await?;
    match method {
        "getIndexes" | "getArtists" => {
            let key = if method == "getIndexes" {
                "indexes"
            } else {
                "artists"
            };
            Ok(
                json!({key:{"ignoredArticles":"The El La Los Las Le Les A An As Es Os","lastModified":OffsetDateTime::now_utc().unix_timestamp()*1000,"index":index(&catalogue)}}),
            )
        }
        "getArtist" => {
            let row = catalogue.artist(id(params.require("id")?)?)?;
            let mut item = artist(row, &catalogue.albums);
            item["album"] = json!(
                catalogue
                    .albums
                    .iter()
                    .filter(|album| album.artist_id == Some(row.id))
                    .map(|row| album(row, &catalogue.tracks, false))
                    .collect::<Vec<_>>()
            );
            Ok(json!({"artist":item}))
        }
        "getAlbum" => {
            let row = catalogue.album(id(params.require("id")?)?)?;
            let mut item = album(row, &catalogue.tracks, false);
            let mut tracks = catalogue
                .tracks
                .iter()
                .filter(|track| track.album_id == Some(row.id))
                .collect::<Vec<_>>();
            tracks.sort_by_key(|track| (track.disc_number, track.track_number, track.id));
            item["song"] = json!(tracks.into_iter().map(song).collect::<Vec<_>>());
            Ok(json!({"album":item}))
        }
        "getMusicDirectory" => directory(&catalogue, id(params.require("id")?)?),
        "getArtistInfo" | "getArtistInfo2" => {
            let row = catalogue.artist(id(params.require("id")?)?)?;
            let key = if method == "getArtistInfo" {
                "artistInfo"
            } else {
                "artistInfo2"
            };
            Ok(
                json!({key:compact(json!({"biography":row.description,"musicBrainzId":row.musicbrainz_id,"similarArtist":[]}))}),
            )
        }
        "getAlbumInfo" | "getAlbumInfo2" => {
            let row = catalogue.album(id(params.require("id")?)?)?;
            let key = if method == "getAlbumInfo" {
                "albumInfo"
            } else {
                "albumInfo2"
            };
            Ok(
                json!({key:compact(json!({"notes":row.description,"musicBrainzId":row.musicbrainz_id}))}),
            )
        }
        "getGenres" => Ok(genres(&catalogue)),
        "getSongsByGenre" => Ok(
            json!({"songsByGenre":{"song":catalogue.songs(params,Some(params.require("genre")?))}}),
        ),
        "getRandomSongs" => {
            let mut songs = catalogue.songs(params, params.get("genre"));
            shuffle(&mut songs);
            Ok(json!({"randomSongs":{"song":songs}}))
        }
        "search" | "search2" | "search3" => Ok(search(&catalogue, method, params)),
        "getAlbumList" | "getAlbumList2" => album_list(&catalogue, method, params),
        _ => Err(Error {
            code: 0,
            message: "Unknown catalogue method".into(),
        }),
    }
}
fn directory(catalogue: &Catalogue, id: i64) -> Result<Value, Error> {
    if let Ok(row) = catalogue.artist(id) {
        return Ok(
            json!({"directory":{"id":id.to_string(),"name":row.name,"child":catalogue.albums.iter().filter(|album|album.artist_id==Some(id)).map(|row|album(row,&catalogue.tracks,true)).collect::<Vec<_>>()}}),
        );
    }
    let row = catalogue.album(id)?;
    let mut tracks = catalogue
        .tracks
        .iter()
        .filter(|track| track.album_id == Some(id))
        .collect::<Vec<_>>();
    tracks.sort_by_key(|track| (track.disc_number, track.track_number, track.id));
    Ok(
        json!({"directory":{"id":id.to_string(),"name":row.name,"parent":row.artist_id.map(|id|id.to_string()),"child":tracks.into_iter().map(song).collect::<Vec<_>>()}}),
    )
}
fn shuffle<T>(items: &mut [T]) {
    for index in (1..items.len()).rev() {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            return;
        }
        let target = usize::try_from(
            u64::from_le_bytes(bytes) % u64::try_from(index + 1).unwrap_or(u64::MAX),
        )
        .unwrap_or(0);
        items.swap(index, target);
    }
}
fn search(catalogue: &Catalogue, method: &str, params: &Params) -> Value {
    let query = params.get("query").unwrap_or_default().to_lowercase();
    let artists = catalogue
        .artists
        .iter()
        .filter(|row| row.name.to_lowercase().contains(&query))
        .skip(params.number("artistOffset", 0, 10000))
        .take(params.number("artistCount", 20, 500))
        .map(|row| artist(row, &catalogue.albums))
        .collect::<Vec<_>>();
    let albums = catalogue
        .albums
        .iter()
        .filter(|row| {
            format!("{} {}", row.name, row.artist)
                .to_lowercase()
                .contains(&query)
        })
        .skip(params.number("albumOffset", 0, 10000))
        .take(params.number("albumCount", 20, 500))
        .map(|row| album(row, &catalogue.tracks, false))
        .collect::<Vec<_>>();
    let songs = catalogue
        .tracks
        .iter()
        .filter(|track| {
            format!("{} {} {}", track.title, track.artist, track.album)
                .to_lowercase()
                .contains(&query)
        })
        .skip(params.number("songOffset", 0, 10000))
        .take(params.number("songCount", 20, 500))
        .map(song)
        .collect::<Vec<_>>();
    if method == "search" {
        legacy_search(catalogue, params)
    } else {
        let key = if method == "search2" {
            "searchResult2"
        } else {
            "searchResult3"
        };
        json!({key:{"artist":artists,"album":albums,"song":songs}})
    }
}
fn legacy_search(catalogue: &Catalogue, params: &Params) -> Value {
    let any = params.get("any").or_else(|| params.get("query"));
    let mut matches = Vec::new();
    if let Some(term) = params.get("artist").or(any) {
        let term = term.to_lowercase();
        matches.extend(
            catalogue
                .artists
                .iter()
                .filter(|row| row.name.to_lowercase().contains(&term))
                .map(|row| json!({"id":row.id.to_string(),"name":row.name,"isDir":true})),
        );
    }
    if let Some(term) = params.get("album").or(any) {
        let term = term.to_lowercase();
        matches.extend(
            catalogue
                .albums
                .iter()
                .filter(|row| row.name.to_lowercase().contains(&term))
                .map(|row| album(row, &catalogue.tracks, true)),
        );
    }
    if let Some(term) = params.get("title").or(any) {
        let term = term.to_lowercase();
        matches.extend(
            catalogue
                .tracks
                .iter()
                .filter(|row| row.title.to_lowercase().contains(&term))
                .skip(params.number("offset", 0, 10000))
                .take(params.number("count", 20, 500))
                .map(song),
        );
    }
    matches.truncate(params.number("count", 20, 500));
    json!({"searchResult":{"match":matches}})
}

fn album_list(catalogue: &Catalogue, method: &str, params: &Params) -> Result<Value, Error> {
    let kind = params.get("type").unwrap_or("newest");
    let mut rows = catalogue.albums.iter().collect::<Vec<_>>();
    match kind {
        "starred" => rows.clear(),
        "newest" | "recent" | "frequent" | "highest" => {
            rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        }
        "alphabeticalByName" => {}
        "alphabeticalByArtist" => rows.sort_by(|a, b| {
            a.artist
                .to_lowercase()
                .cmp(&b.artist.to_lowercase())
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
        "random" => shuffle(&mut rows),
        "byYear" => {
            let from = params
                .require("fromYear")?
                .parse::<i32>()
                .map_err(|_| Error {
                    code: 10,
                    message: "Invalid fromYear".into(),
                })?;
            let to = params
                .require("toYear")?
                .parse::<i32>()
                .map_err(|_| Error {
                    code: 10,
                    message: "Invalid toYear".into(),
                })?;
            rows.retain(|row| {
                row.release_year
                    .is_some_and(|year| year >= from.min(to) && year <= from.max(to))
            });
            rows.sort_by_key(|row| row.release_year);
            if from > to {
                rows.reverse();
            }
        }
        "byGenre" => {
            let genre = params.require("genre")?;
            rows.retain(|row| {
                row.genres
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(genre))
                    || catalogue.tracks.iter().any(|track| {
                        track.album_id == Some(row.id) && track.genre.eq_ignore_ascii_case(genre)
                    })
            });
        }
        _ => {
            return Err(Error {
                code: 0,
                message: "Unknown album list type".into(),
            });
        }
    }
    let key = if method == "getAlbumList" {
        "albumList"
    } else {
        "albumList2"
    };
    Ok(
        json!({key:{"album":rows.into_iter().skip(params.number("offset",0,10000)).take(params.number("size",10,500)).map(|row|album(row,&catalogue.tracks,method=="getAlbumList")).collect::<Vec<_>>()}}),
    )
}

fn genres(catalogue: &Catalogue) -> Value {
    let mut groups: BTreeMap<String, (usize, HashSet<i64>)> = BTreeMap::new();
    for track in &catalogue.tracks {
        if !track.genre.is_empty() {
            let (count, albums) = groups.entry(track.genre.clone()).or_default();
            *count += 1;
            if let Some(album) = track.album_id {
                albums.insert(album);
            }
        }
    }
    json!({"genres":{"genre":groups.into_iter().map(|(value,(count,albums))|json!({"value":value,"songCount":count,"albumCount":albums.len()})).collect::<Vec<_>>()}})
}
