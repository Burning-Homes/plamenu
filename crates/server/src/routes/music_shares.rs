//! Music share management and strictly scoped capability media reads.
use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    music_saved::Reference,
    music_shares::{self, New},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
#[derive(Default, serde::Deserialize)]
struct Page {
    before: Option<String>,
    #[serde(default)]
    include_revoked: bool,
}
#[derive(Default, serde::Deserialize)]
pub struct Access {
    #[serde(default)]
    pub token: String,
    pub format: Option<String>,
    pub bitrate: Option<u32>,
}
pub fn private_response(value: impl serde::Serialize) -> Response {
    harden(Json(value).into_response())
}
pub fn harden(mut response: Response) -> Response {
    for (key, value) in [
        (header::CACHE_CONTROL, "private, no-store"),
        (header::VARY, "Authorization, Cookie"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ] {
        response.headers_mut().insert(key, value.parse().unwrap());
    }
    response
        .headers_mut()
        .insert("x-robots-tag", "noindex, noarchive".parse().unwrap());
    response
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/shares", get(index))
        .route("/api/v1/music/share-links", axum::routing::post(create))
        .route(
            "/api/v1/music/share-links/{id}",
            axum::routing::delete(revoke),
        )
        .route(
            "/api/v1/music/shares/grants/{member}",
            axum::routing::delete(revoke_grant),
        )
}
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/music/share/assets/{id}/stream", get(stream))
        .route("/music/share/assets/{id}/download", get(download))
        .route("/music/share/artwork/{id}", get(artwork))
}
async fn index(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(page): Query<Page>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(private_response(
        music_shares::page(
            &state,
            current.account.id,
            page.before.as_deref(),
            page.include_revoked,
        )
        .await?,
    ))
}
async fn create(
    State(state): State<AppState>,
    current: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input: New = super::params::parse_body(&headers, &body)?;
    Ok(private_response(
        music_shares::create(&state, current.account.id, &input).await?,
    ))
}
async fn revoke(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_shares::revoke(&state, current.account.id, id).await?;
    Ok(private_response(serde_json::json!({})))
}
async fn revoke_grant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(member): Path<i64>,
    Query(reference): Query<Reference>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_shares::revoke_grant(&state, current.account.id, &reference, member).await?;
    Ok(private_response(serde_json::json!({})))
}
async fn serve(
    state: &AppState,
    id: i64,
    access: &Access,
    viewer: Option<i64>,
    headers: &HeaderMap,
    original: bool,
) -> Result<Response, ApiError> {
    let scope = music_shares::resolve(state, &access.token, viewer).await?;
    let asset = music_shares::asset(state, &scope, id, viewer).await?;
    let source =
        crate::music_bytes::Source::asset(state, &asset, viewer, Some(scope.id), original).await?;
    let (reader, original_format) = if original {
        let info = crate::music_original::open_info(state, &asset).await?;
        (info.reader, info.format)
    } else {
        (
            state
                .media
                .open(&asset.stream_file)
                .await
                .map_err(|_| ApiError::NotFound)?,
            asset.original_format.clone(),
        )
    };
    let mut response = super::media::stream_open_file(
        reader,
        if original {
            "application/octet-stream"
        } else {
            "audio/mpeg"
        },
        "private, no-store",
        headers,
    )
    .await?;
    if original {
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"track-{id}.{original_format}\"")
                .parse()
                .unwrap(),
        );
    }
    crate::music_bytes::wrap(state, response, Some(source), None).await
}
async fn stream(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(access): Query<Access>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    let credential = token
        .as_ref()
        .or_else(|| web.as_ref().map(|u| &u.current))
        .map(crate::music_bytes::Credential::from_user);
    let result = async {
        if let Some(user) = &token {
            user.require_scope("read:statuses")?;
        }
        let viewer = token
            .as_ref()
            .map(|u| u.account.id)
            .or_else(|| web.as_ref().map(|u| u.current.account.id));
        let scope = music_shares::resolve(&state, &access.token, viewer).await?;
        let asset = music_shares::asset(&state, &scope, id, viewer).await?;
        let format = access
            .format
            .clone()
            .or_else(|| asset.indexed_original.then(|| "mp3".into()));
        if let Some(format) = &format {
            let viewer = token
                .as_ref()
                .map(|u| u.account.id)
                .or_else(|| web.as_ref().map(|u| u.current.account.id));
            let scope = music_shares::resolve(&state, &access.token, viewer).await?;
            if method == axum::http::Method::HEAD {
                return crate::music_renditions::cold::head(
                    &state,
                    id,
                    crate::music_renditions::cold::Access {
                        viewer,
                        scope: Some(scope),
                    },
                    format,
                    access.bitrate,
                    &headers,
                )
                .await;
            }
            return crate::music_renditions::cold::serve(
                &state,
                id,
                crate::music_renditions::cold::Access {
                    viewer,
                    scope: Some(scope),
                },
                format,
                access.bitrate,
                &headers,
            )
            .await;
        }
        serve(
            &state,
            id,
            &access,
            token
                .as_ref()
                .map(|u| u.account.id)
                .or_else(|| web.as_ref().map(|u| u.current.account.id)),
            &headers,
            false,
        )
        .await
    }
    .await;
    let result = match result {
        Ok(response) => crate::music_bytes::wrap(&state, response, None, credential).await,
        Err(e) => Err(e),
    };
    harden(result.unwrap_or_else(IntoResponse::into_response))
}
async fn download(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(access): Query<Access>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Response {
    let credential = token
        .as_ref()
        .or_else(|| web.as_ref().map(|u| &u.current))
        .map(crate::music_bytes::Credential::from_user);
    let result = async {
        if let Some(user) = &token {
            user.require_scope("read:statuses")?;
        }
        serve(
            &state,
            id,
            &access,
            token
                .as_ref()
                .map(|u| u.account.id)
                .or_else(|| web.as_ref().map(|u| u.current.account.id)),
            &headers,
            true,
        )
        .await
    }
    .await;
    let result = match result {
        Ok(response) => crate::music_bytes::wrap(&state, response, None, credential).await,
        Err(e) => Err(e),
    };
    harden(result.unwrap_or_else(IntoResponse::into_response))
}
async fn artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(access): Query<Access>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Response {
    let result = async {
        if let Some(user) = &token {
            user.require_scope("read:statuses")?;
        }
        let viewer = token
            .as_ref()
            .map(|u| u.account.id)
            .or_else(|| web.as_ref().map(|u| u.current.account.id));
        let scope = music_shares::resolve(&state, &access.token, viewer).await?;
        let art = music_shares::artwork(&state, &scope, id, viewer).await?;
        let response = super::media::stream_stored_file(
            &state,
            &art.file_name,
            &art.content_type,
            "private, no-store",
            &headers,
        )
        .await?;
        let credential = token
            .as_ref()
            .or_else(|| web.as_ref().map(|u| &u.current))
            .map(crate::music_bytes::Credential::from_user);
        crate::music_bytes::wrap(
            &state,
            response,
            Some(crate::music_bytes::Source::Artwork {
                id,
                viewer,
                scope: Some(scope.id),
                file: art.file_name,
            }),
            credential,
        )
        .await
    }
    .await;
    harden(result.unwrap_or_else(IntoResponse::into_response))
}
