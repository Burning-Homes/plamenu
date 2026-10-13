//! Bounded media reads retain both source and client authority after admission.
use super::{
    catalogue, playlists,
    protocol::{Error, Params},
};
use crate::{AppState, error::ApiError, music_clients::auth::Principal, music_collections::Entry};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, header},
    response::Response,
};
use futures_util::StreamExt as _;
#[derive(Clone)]
enum Proof {
    Asset(i64, String),
    Status(i64),
    Artwork(i64),
    Avatar(i64, String),
    Remote(i64),
}
struct Relay {
    state: AppState,
    principal: Principal,
    proof: Proof,
    body: axum::body::BodyDataStream,
    checked: std::time::Instant,
}
impl Proof {
    async fn current(&self, state: &AppState, owner: i64) -> Result<(), ApiError> {
        match self {
            Self::Asset(id, hash) => {
                let row = crate::music_assets::get(state, *id, Some(owner)).await?;
                if &row.audio_hash != hash {
                    return Err(ApiError::NotFound);
                }
            }
            Self::Status(id) => {
                let entry = Entry {
                    id: 1,
                    collection_id: 0,
                    position: 1,
                    status_id: Some(*id),
                    asset_id: None,
                    remote_id: None,
                    episode_id: None,
                };
                if crate::music_collections::sources(state, &[entry], Some(owner))
                    .await?
                    .is_empty()
                {
                    return Err(ApiError::NotFound);
                }
            }
            Self::Artwork(id) => {
                crate::music_artworks::get(state, *id, Some(owner)).await?;
            }
            Self::Avatar(id, key) => {
                let account = avatar_account(state, owner, *id).await?;
                if account.avatar_file_name.as_ref() != Some(key) {
                    return Err(ApiError::NotFound);
                }
            }
            Self::Remote(id) => {
                crate::music_remote::stream::source(state, *id, owner).await?;
            }
        }
        Ok(())
    }
}
async fn next(mut relay: Relay) -> Result<Option<(Bytes, Relay)>, std::io::Error> {
    if relay.state.shutdown.is_cancelled() {
        return Ok(None);
    }
    if relay.checked.elapsed() >= std::time::Duration::from_secs(1) {
        relay
            .principal
            .current(&relay.state)
            .await
            .map_err(std::io::Error::other)?;
        relay
            .proof
            .current(&relay.state, relay.principal.owner)
            .await
            .map_err(std::io::Error::other)?;
        relay.checked = std::time::Instant::now();
    }
    let chunk = relay
        .body
        .next()
        .await
        .transpose()
        .map_err(std::io::Error::other)?;
    if relay.checked.elapsed() >= std::time::Duration::from_secs(1) {
        relay
            .principal
            .current(&relay.state)
            .await
            .map_err(std::io::Error::other)?;
        relay
            .proof
            .current(&relay.state, relay.principal.owner)
            .await
            .map_err(std::io::Error::other)?;
        relay.checked = std::time::Instant::now();
    }
    Ok(chunk.map(|bytes| (bytes, relay)))
}
fn guard(state: &AppState, principal: Principal, proof: Proof, response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        header::CACHE_CONTROL,
        "private, no-store".parse().expect("static header"),
    );
    parts.headers.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header"),
    );
    let relay = Relay {
        state: state.clone(),
        principal,
        proof,
        body: body.into_data_stream(),
        checked: std::time::Instant::now(),
    };
    Response::from_parts(
        parts,
        Body::from_stream(futures_util::stream::try_unfold(relay, next)),
    )
}
pub async fn dispatch(
    state: &AppState,
    principal: Principal,
    endpoint: &str,
    params: &Params,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response, Error> {
    principal.current(state).await?;
    match endpoint {
        "stream" | "download" => audio(state, principal, endpoint, params, method, headers).await,
        "getCoverArt" => cover(state, principal, params, headers).await,
        "getAvatar" => avatar(state, principal, params, headers).await,
        _ => Err(Error {
            code: 0,
            message: "Unknown media method".into(),
        }),
    }
}
async fn audio(
    state: &AppState,
    principal: Principal,
    endpoint: &str,
    params: &Params,
    method: Method,
    headers: &HeaderMap,
) -> Result<Response, Error> {
    let source = playlists::source(params.require("id")?)?;
    let mut entry = source.entry(1, 0, 1)?;
    crate::music_remote::normalize_sources(
        state,
        std::slice::from_mut(&mut entry),
        principal.owner,
    )
    .await?;
    let (response, proof) = if let Some(id) = entry.asset_id {
        let row = crate::music_assets::get(state, id, Some(principal.owner)).await?;
        let response = asset_audio(
            state,
            principal.owner,
            endpoint,
            params,
            &method,
            headers,
            &row,
        )
        .await?;
        (response, Proof::Asset(id, row.audio_hash))
    } else if let Some(remote) = entry.remote_id {
        let access = crate::music_remote::stream::Access::Client { principal };
        let response = if method == Method::HEAD {
            let source =
                crate::music_remote::stream::source(state, remote, principal.owner).await?;
            let mut response = Response::new(Body::empty());
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                source
                    .media
                    .details
                    .audio_type
                    .as_deref()
                    .unwrap_or("audio/mpeg")
                    .parse()
                    .map_err(|_| ApiError::NotFound)?,
            );
            response
        } else {
            crate::music_remote::stream::serve(state, remote, access, headers).await?
        };
        (response, Proof::Remote(remote))
    } else if let Some(status) = entry.status_id {
        let playables = crate::music_collections::sources(
            state,
            std::slice::from_ref(&entry),
            Some(principal.owner),
        )
        .await?;
        if playables.is_empty() {
            return Err(ApiError::NotFound.into());
        }
        let mut rows = plamenu_db::media::for_statuses(&state.pool, &[status])
            .await
            .map_err(ApiError::from)?;
        let row = rows
            .remove(&status)
            .and_then(|rows| {
                rows.into_iter()
                    .find(|row| row.content_type.starts_with("audio/"))
            })
            .ok_or(ApiError::NotFound)?;
        let key = row.file_name.ok_or(ApiError::NotFound)?;
        (
            crate::routes::media::stream_stored_file(
                state,
                &key,
                &row.content_type,
                "private, no-store",
                headers,
            )
            .await?,
            Proof::Status(status),
        )
    } else {
        return Err(ApiError::NotFound.into());
    };
    let mut response = guard(state, principal, proof, response);
    if endpoint == "download" {
        let filename = download_name(&response);
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            filename.parse().expect("generated download header"),
        );
    }
    Ok(response)
}
async fn cover(
    state: &AppState,
    principal: Principal,
    params: &Params,
    headers: &HeaderMap,
) -> Result<Response, Error> {
    let raw = params.require("id")?;
    let (kind, id) = raw
        .split_once('-')
        .map_or(("tr", raw), |(kind, id)| (kind, id));
    let id = catalogue::id(id)?;
    let artwork = match kind {
        "tr" => crate::music_assets::get(state, id, Some(principal.owner))
            .await?
            .artwork_id
            .ok_or(ApiError::NotFound)?,
        "ar" | "al" | "pl" => {
            let target = match kind {
                "ar" => crate::music_artworks::Target::Artists,
                "al" => crate::music_artworks::Target::Albums,
                _ => crate::music_artworks::Target::Collections,
            };
            crate::music_artworks::for_target(state, target, id, Some(principal.owner))
                .await?
                .into_iter()
                .next()
                .ok_or(ApiError::NotFound)?
                .id
        }
        "remote" => {
            return Ok(crate::music_remote::artwork::serve(
                state,
                id,
                crate::music_remote::stream::Access::Client { principal },
            )
            .await?);
        }
        _ => return Err(ApiError::NotFound.into()),
    };
    let row = crate::music_artworks::get(state, artwork, Some(principal.owner)).await?;
    let response = crate::routes::media::stream_stored_file(
        state,
        &row.file_name,
        &row.content_type,
        "private, no-store",
        headers,
    )
    .await?;
    Ok(guard(state, principal, Proof::Artwork(artwork), response))
}
async fn avatar_account(
    state: &AppState,
    owner: i64,
    id: i64,
) -> Result<plamenu_db::account::Account, ApiError> {
    let account = plamenu_db::account::find_by_id(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if account.domain.is_some()
        || !crate::instance_policy::public_account_visible(
            &state.pool,
            &state.config.domain,
            &account,
        )
        .await?
        || plamenu_db::block::exists(&state.pool, owner, id).await?
        || plamenu_db::block::exists(&state.pool, id, owner).await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(account)
}
async fn avatar(
    state: &AppState,
    principal: Principal,
    params: &Params,
    headers: &HeaderMap,
) -> Result<Response, Error> {
    let username = params.require("username")?;
    let account = plamenu_db::account::find_local_by_username(&state.pool, username)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let account = avatar_account(state, principal.owner, account.id).await?;
    let key = account.avatar_file_name.ok_or(ApiError::NotFound)?;
    let mime = match key.rsplit('.').next() {
        Some("avif") => "image/avif",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "image/webp",
    };
    let response =
        crate::routes::media::stream_stored_file(state, &key, mime, "private, no-store", headers)
            .await?;
    Ok(guard(
        state,
        principal,
        Proof::Avatar(account.id, key),
        response,
    ))
}

async fn asset_audio(
    state: &AppState,
    owner: i64,
    endpoint: &str,
    params: &Params,
    method: &Method,
    headers: &HeaderMap,
    row: &crate::music_assets::Asset,
) -> Result<Response, ApiError> {
    let format = params
        .get("format")
        .filter(|value| !value.is_empty() && *value != "raw");
    let bitrate = params
        .get("maxBitRate")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    if endpoint == "stream"
        && params.get("format") != Some("raw")
        && (format.is_some() || bitrate.is_some())
    {
        let access = crate::music_renditions::cold::Access {
            viewer: Some(owner),
            scope: None,
        };
        if *method == Method::HEAD {
            crate::music_renditions::cold::head(
                state,
                row.id,
                access,
                format.unwrap_or("mp3"),
                bitrate,
                headers,
            )
            .await
        } else {
            crate::music_renditions::cold::serve(
                state,
                row.id,
                access,
                format.unwrap_or("mp3"),
                bitrate,
                headers,
            )
            .await
        }
    } else {
        let original = crate::music_original::open_info(state, row).await?;
        crate::routes::media::stream_open_file(
            original.reader,
            catalogue::mime(&original.format),
            "private, no-store",
            headers,
        )
        .await
    }
}

fn download_name(response: &Response) -> String {
    let kind = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default();
    let extension = match kind {
        "audio/mpeg" => "mp3",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        _ => "audio",
    };
    format!("attachment; filename=\"music-download.{extension}\"")
}
