//! Music asset API and protected range serving.
use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    music_assets,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/music/assets/{id}/publication",
            axum::routing::post(publish).delete(withdraw),
        )
        .route(
            "/api/v1/music/assets/{id}/details",
            axum::routing::put(details),
        )
        .route(
            "/api/v1/music/assets/{id}/album",
            axum::routing::put(set_album),
        )
        .route("/api/v1/music/assets", get(list).post(import))
        .route(
            "/api/v1/music/assets/{id}",
            get(show).put(update).delete(delete),
        )
        .route(
            "/api/v1/music/assets/{id}/grants/{member}",
            axum::routing::put(grant).delete(revoke),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::media_processing::MAX_AV_UPLOAD_BYTES + 1024 * 1024,
        ))
}
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/music/assets/{id}/stream", get(stream))
        .route("/music/assets/{id}/download", get(download))
}
fn json_response(value: Value) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
#[derive(Default, Deserialize)]
struct List {
    q: Option<String>,
    mine: Option<bool>,
    max_id: Option<i64>,
}
async fn list(
    State(state): State<AppState>,
    current: CurrentUser,
    Query(q): Query<List>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let mut items = music_assets::catalogue(
        &state,
        Some(current.account.id),
        q.mine.unwrap_or(false),
        q.q.as_deref().unwrap_or_default(),
        q.max_id,
    )
    .await?;
    let before = if items.len() > 60 {
        items.truncate(60);
        items.last().map(|v| v.id.to_string())
    } else {
        None
    };
    Ok(json_response(
        json!({"items":items.iter().map(music_assets::Asset::entity).collect::<Vec<_>>(),"before":before}),
    ))
}
async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    Ok(json_response(
        music_assets::get(&state, id, Some(current.account.id))
            .await?
            .entity(),
    ))
}
async fn import(
    State(state): State<AppState>,
    current: CurrentUser,
    mut form: Multipart,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let mut upload = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::BadRequest("Invalid audio upload".into()))?
    {
        if field.name() == Some("file") {
            if upload.is_some() {
                return Err(ApiError::Unprocessable("Upload one track at a time".into()));
            }
            let name = field.file_name().unwrap_or("Untitled track").to_owned();
            let bytes = field
                .bytes()
                .await
                .map_err(|_| ApiError::BadRequest("Invalid audio upload".into()))?;
            upload = Some((name, bytes.to_vec()));
        }
    }
    let (name, bytes) =
        upload.ok_or_else(|| ApiError::Unprocessable("Choose an audio file".into()))?;
    Ok(json_response(
        music_assets::import(&state, current.account.id, bytes, &name)
            .await?
            .entity(),
    ))
}
async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(json_response(
        music_assets::update(&state, current.account.id, id, &input)
            .await?
            .entity(),
    ))
}
async fn delete(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_assets::delete(&state, current.account.id, id).await?;
    Ok(json_response(json!({})))
}
async fn grant(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_assets::grant(&state, current.account.id, id, member, true).await?;
    Ok(json_response(json!({})))
}
async fn revoke(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((id, member)): Path<(i64, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    music_assets::grant(&state, current.account.id, id, member, false).await?;
    Ok(json_response(json!({})))
}
async fn serve(
    state: &AppState,
    id: i64,
    token: Option<CurrentUser>,
    web: Option<crate::web::session::WebUser>,
    headers: &HeaderMap,
    original: bool,
) -> Response {
    let result = async {
        if let Some(user) = &token {
            user.require_scope("read:statuses")?;
        }
        let viewer = token
            .as_ref()
            .map(|u| u.account.id)
            .or_else(|| web.as_ref().map(|u| u.current.account.id));
        let asset = music_assets::get(state, id, viewer).await?;
        let source =
            crate::music_bytes::Source::asset(state, &asset, viewer, None, original).await?;
        let credential = token
            .as_ref()
            .or_else(|| web.as_ref().map(|u| &u.current))
            .map(crate::music_bytes::Credential::from_user);
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
        crate::music_bytes::wrap(state, response, Some(source), credential).await
    }
    .await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::VARY, "Authorization, Cookie".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn stream(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    Query(options): Query<StreamOptions>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    let indexed = music_assets::get(&state, id, viewer)
        .await
        .is_ok_and(|asset| asset.indexed_original);
    let credential = token
        .as_ref()
        .or_else(|| web.as_ref().map(|u| &u.current))
        .map(crate::music_bytes::Credential::from_user);
    if let Some(format) = options.format.or_else(|| indexed.then(|| "mp3".into())) {
        let result = async {
            if let Some(token) = &token {
                token.require_scope("read:statuses")?;
            }
            let viewer = token
                .as_ref()
                .map(|u| u.account.id)
                .or_else(|| web.as_ref().map(|u| u.current.account.id));
            let access = crate::music_renditions::cold::Access {
                viewer,
                scope: None,
            };
            if method == axum::http::Method::HEAD {
                return crate::music_renditions::cold::head(
                    &state,
                    id,
                    access,
                    &format,
                    options.bitrate,
                    &headers,
                )
                .await;
            }
            let response = crate::music_renditions::cold::serve(
                &state,
                id,
                access,
                &format,
                options.bitrate,
                &headers,
            )
            .await?;
            crate::music_bytes::wrap(&state, response, None, credential).await
        }
        .await;
        return result.unwrap_or_else(IntoResponse::into_response);
    }
    serve(&state, id, token, web, &headers, false).await
}
#[derive(Default, Deserialize)]
struct StreamOptions {
    format: Option<String>,
    bitrate: Option<u32>,
}
async fn download(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Response {
    serve(&state, id, token, web, &headers, true).await
}

async fn publish(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    current.require_scope("write:media")?;
    let status = crate::music_publications::publish(&state, &current.account, id).await?;
    Ok(json_response(
        crate::entities::render_status(
            &state.pool,
            &state.config.domain,
            &status,
            Some(current.account.id),
        )
        .await?,
    ))
}
async fn withdraw(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    current.require_scope("write:statuses")?;
    current.require_scope("write:media")?;
    crate::music_publications::withdraw(&state, &current.account, id).await?;
    Ok(json_response(json!({})))
}

async fn details(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(json_response(
        crate::music_track_details::update(&state, current.account.id, id, &input)
            .await?
            .entity(),
    ))
}
async fn set_album(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input = super::params::parse_body(&headers, &body)?;
    Ok(json_response(
        crate::music_track_details::set_album(&state, current.account.id, id, &input)
            .await?
            .entity(),
    ))
}
