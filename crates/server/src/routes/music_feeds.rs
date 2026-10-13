//! Native syndication endpoints authorize sources before conditional responses.
use crate::{AppState, auth::MaybeUser, error::ApiError, music_feeds};
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
#[derive(Default, Deserialize)]
struct Options {
    limit: Option<usize>,
    token: Option<String>,
}
pub fn router() -> Router<AppState> {
    Router::new().route("/music/feeds/{kind}/{key}", get(index))
}
async fn index(
    State(state): State<AppState>,
    Path((kind, key)): Path<(String, String)>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
    Query(options): Query<Options>,
) -> Result<Response, ApiError> {
    if let Some(user) = &token {
        user.require_scope("read:statuses")?;
    }
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    let (id, format) = key.rsplit_once('.').ok_or(ApiError::NotFound)?;
    let format = music_feeds::Format::parse(format)?;
    let feed = match kind.as_str() {
        "collections" => {
            let id = id.parse().map_err(|_| ApiError::NotFound)?;
            if let Some(token) = options.token.as_deref() {
                music_feeds::shared_collection(
                    &state,
                    id,
                    viewer,
                    options.limit.unwrap_or(50),
                    token,
                )
                .await?
            } else {
                music_feeds::collection(&state, id, viewer, options.limit.unwrap_or(50)).await?
            }
        }
        "profiles" => music_feeds::profile(&state, id, viewer, options.limit.unwrap_or(50)).await?,
        "artists" => {
            music_feeds::artist(
                &state,
                id.parse().map_err(|_| ApiError::NotFound)?,
                viewer,
                options.limit.unwrap_or(50),
            )
            .await?
        }
        "tags" => {
            music_feeds::term(
                &state,
                music_feeds::Term::Tag,
                id,
                viewer,
                options.limit.unwrap_or(50),
            )
            .await?
        }
        "genres" => {
            music_feeds::term(
                &state,
                music_feeds::Term::Genre,
                id,
                viewer,
                options.limit.unwrap_or(50),
            )
            .await?
        }
        _ => return Err(ApiError::NotFound),
    };
    if options.token.is_some() && kind != "collections" {
        return Err(ApiError::NotFound);
    }
    let mut url = format!("https://{}/music/feeds/{kind}/{key}", state.config.domain);
    if let Some(token) = options.token.as_deref() {
        url.push_str("?token=");
        url.push_str(&url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>());
    }
    let body = feed.render(format, &url);
    let mut etag = String::from("\"");
    for byte in Sha256::digest(body.as_bytes()) {
        let _ = write!(etag, "{byte:02x}");
    }
    etag.push('"');
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|raw| {
            raw.split(',').any(|entry| {
                entry.trim() == etag
                    || entry.trim() == "*"
                    || entry.trim().strip_prefix("W/") == Some(etag.as_str())
            })
        });
    let headers = [
        (header::CONTENT_TYPE, format.content_type()),
        (header::CACHE_CONTROL, "private, no-cache"),
        (header::VARY, "Authorization, Cookie"),
        (header::ETAG, etag.as_str()),
    ];
    let response = if unchanged {
        (StatusCode::NOT_MODIFIED, headers).into_response()
    } else {
        (headers, body).into_response()
    };
    Ok(if options.token.is_some() {
        super::music_shares::harden(response)
    } else {
        response
    })
}
