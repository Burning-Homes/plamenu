//! Current-authority serving. Private copy jobs never become media endpoints.
use crate::{AppState, error::ApiError};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use plamenu_db::video_mirrors as db;
use serde::Deserialize;
use std::collections::HashMap;
use tokio::io::AsyncReadExt;

#[derive(Default, Deserialize)]
pub struct Options {
    #[serde(default)]
    pub canonical: bool,
}

pub async fn document(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    let item = db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?;
    if item.state != "ready" || !super::authorized(&state, &item).await? {
        return Err(ApiError::NotFound);
    }
    let activity = item.create_activity.ok_or(ApiError::NotFound)?;
    let mut object = activity["object"].clone();
    object["@context"] = super::cache_context();
    Ok((
        [
            (header::CONTENT_TYPE, plamenu_ap::ACTIVITY_JSON_UTF8),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        axum::Json(object),
    )
        .into_response())
}

pub async fn asset(
    State(state): State<AppState>,
    Path((id, name)): Path<(i64, String)>,
    Query(options): Query<Options>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !super::manifest::valid_name(&name) {
        return Err(ApiError::NotFound);
    }
    let item = db::find(&state.pool, id).await?.ok_or(ApiError::NotFound)?;
    if item.state != "ready" || !super::authorized(&state, &item).await? {
        return Err(ApiError::NotFound);
    }
    let assets = db::assets(&state.pool, id, item.generation).await?;
    let stored = assets
        .iter()
        .find(|asset| asset.public_name == name)
        .ok_or(ApiError::NotFound)?;
    if std::path::Path::new(&name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
        && !options.canonical
    {
        let opened = state
            .media
            .open(&stored.file_name)
            .await
            .map_err(|_| ApiError::NotFound)?;
        if opened.len > super::manifest::MAX_PLAYLIST_BYTES as u64 {
            return Err(ApiError::NotFound);
        }
        let mut body = String::new();
        opened
            .reader
            .take(super::manifest::MAX_PLAYLIST_BYTES as u64 + 1)
            .read_to_string(&mut body)
            .await
            .map_err(|_| ApiError::NotFound)?;
        let base = super::https(&stored.source_url)?;
        let names = assets
            .iter()
            .map(|asset| (asset.source_url.clone(), asset.public_name.clone()))
            .collect::<HashMap<_, _>>();
        let body =
            super::manifest::rewrite(&base, &body, &names).map_err(|_| ApiError::NotFound)?;
        return Ok((
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::CACHE_CONTROL, "private, no-store"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            body,
        )
            .into_response());
    }
    let mut response = crate::routes::media::stream_stored_file(
        &state,
        &stored.file_name,
        &stored.content_type,
        "private, no-store",
        &headers,
    )
    .await?;
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "Content-Length, Content-Range, Accept-Ranges"
            .parse()
            .unwrap(),
    );
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    Ok(response)
}
