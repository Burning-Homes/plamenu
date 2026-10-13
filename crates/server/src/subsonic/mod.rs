//! `OpenSubsonic` compatibility reuses Music ownership and authorization services.
mod annotations;
mod catalogue;
mod favourites;
mod media;
mod playlists;
mod plays;
mod protocol;
use crate::{
    AppState,
    error::ApiError,
    music_clients::auth::{self, Credentials, Principal},
};
use axum::{
    Router,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State},
    http::{Method, header},
    response::Response,
    routing::get,
};
use protocol::{Error, Params};
use serde_json::{Value, json};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rest/{method}", get(dispatch).post(dispatch))
        .layer(DefaultBodyLimit::max(65536))
}
async fn params(state: &AppState, request: Request) -> Result<Params, ApiError> {
    let mut params = Params(
        serde_urlencoded::from_str(request.uri().query().unwrap_or_default())
            .map_err(|_| ApiError::BadRequest("Invalid Subsonic parameters".into()))?,
    );
    if request.method() == Method::POST {
        let kind = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if kind.starts_with("multipart/form-data") {
            let mut multipart = Multipart::from_request(request, state)
                .await
                .map_err(|_| ApiError::BadRequest("Invalid Subsonic form".into()))?;
            while let Some(field) = multipart
                .next_field()
                .await
                .map_err(|_| ApiError::BadRequest("Invalid Subsonic form".into()))?
            {
                if let Some(name) = field.name().map(str::to_owned) {
                    let value = field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("Invalid Subsonic form".into()))?;
                    params.0.push((name, value));
                }
                if params.0.len() > 1000 {
                    return Err(ApiError::BadRequest("Too many Subsonic parameters".into()));
                }
            }
        } else if kind.starts_with("application/x-www-form-urlencoded") {
            let bytes = axum::body::to_bytes(request.into_body(), 65536)
                .await
                .map_err(|_| ApiError::BadRequest("Invalid Subsonic form".into()))?;
            let form: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes)
                .map_err(|_| ApiError::BadRequest("Invalid Subsonic form".into()))?;
            params.0.extend(form);
        }
    }
    if params.0.len() > 1000 {
        return Err(ApiError::BadRequest("Too many Subsonic parameters".into()));
    }
    Ok(params)
}
async fn authenticate(
    state: &AppState,
    params: &Params,
    ip: Option<std::net::IpAddr>,
) -> Result<Principal, Error> {
    let username = params.require("u")?;
    if params.get("t").is_some() || params.get("s").is_some() {
        params.require("t")?;
        params.require("s")?;
    } else if params.get("p").is_none() && params.get("apiKey").is_none() {
        return Err(Error {
            code: 10,
            message: "No authentication credentials provided; pass p or apiKey".into(),
        });
    }
    let principal = auth::authenticate_at(
        state,
        &Credentials {
            username,
            password: params.get("p"),
            api_key: params.get("apiKey"),
            token: params.get("t"),
            salt: params.get("s"),
        },
        ip,
    )
    .await?;
    protocol::version(params)?;
    principal.current(state).await?;
    Ok(principal)
}
async fn dispatch(
    State(state): State<AppState>,
    Path(endpoint): Path<String>,
    ip: crate::instance_policy::RemoteIp,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let headers = request.headers().clone();
    let params = match params(&state, request).await {
        Ok(params) => params,
        Err(error) => return protocol::render(&Params::default(), Err(error.into())),
    };
    let authorized = async {
        crate::instance_policy::ensure_ip_login_allowed(&state.pool, ip.0).await?;
        authenticate(&state, &params, ip.0).await
    }
    .await;
    let principal = match authorized {
        Ok(principal) => principal,
        Err(error) => return protocol::render(&params, Err(error)),
    };
    let Some(endpoint) = endpoint.strip_suffix(".view") else {
        return protocol::render(
            &params,
            Err(Error {
                code: 0,
                message: "Unknown Subsonic method".into(),
            }),
        );
    };
    if matches!(
        endpoint,
        "stream" | "download" | "getCoverArt" | "getAvatar"
    ) {
        return match media::dispatch(&state, principal, endpoint, &params, method, &headers).await {
            Ok(response) => response,
            Err(error) => protocol::render(&params, Err(error)),
        };
    }
    let result = async {
        let value = basic(&state, &principal, endpoint, &params).await?;
        Ok(annotations::songs(&state, principal.owner, value).await?)
    }
    .await;
    protocol::render(&params, result)
}

async fn basic(
    state: &AppState,
    principal: &Principal,
    method: &str,
    params: &Params,
) -> Result<Value, Error> {
    Ok(match method {
        "ping" | "setRating" | "savePlayQueue" => json!({}),
        "getLicense" => {
            let user = plamenu_db::user::find_by_account_id(&state.pool, principal.owner)
                .await
                .map_err(ApiError::from)?
                .ok_or(ApiError::NotFound)?;
            json!({"license":{"valid":true,"email":user.email.unwrap_or_default()}})
        }
        "getOpenSubsonicExtensions" => {
            json!({"openSubsonicExtensions":{"extension":[{"name":"apiKeyAuthentication","version":[1]},{"name":"formPost","version":[1]}]}})
        }
        "getPlayQueue" => json!({"playQueue":{}}),
        "getVideos" => json!({"videos":{"video":[]}}),
        "getBookmarks" => json!({"bookmarks":{"bookmark":[]}}),
        "getLyrics" | "getLyricsBySongId" => json!({"lyrics":{}}),
        "star" | "unstar" | "getStarred" | "getStarred2" => {
            return favourites::dispatch(state, principal, method, params).await;
        }
        "scrobble" | "nowPlaying" | "getNowPlaying" => {
            return plays::dispatch(state, principal, method, params).await;
        }
        "getPlaylists" | "getPlaylist" | "createPlaylist" | "updatePlaylist" | "deletePlaylist" => {
            return playlists::dispatch(state, principal, method, params).await;
        }
        "getMusicFolders" | "getArtists" | "getIndexes" | "getMusicDirectory" | "getArtist"
        | "getAlbum" | "getSong" | "getArtistInfo" | "getArtistInfo2" | "getAlbumInfo"
        | "getAlbumInfo2" | "getGenres" | "getSongsByGenre" | "getRandomSongs" | "search"
        | "search2" | "search3" | "getAlbumList" | "getAlbumList2" => {
            return catalogue::dispatch(state, principal.owner, method, params).await;
        }
        _ => {
            return Err(Error {
                code: 0,
                message: "Method is not implemented".into(),
            });
        }
    })
}
