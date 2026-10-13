//! Artwork APIs and serving always use the target's current authorization.
use crate::{
    AppState,
    auth::{CurrentUser, MaybeUser},
    error::ApiError,
    music_artworks::{self, Target},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/{kind}/{id}/artworks", get(list))
        .route(
            "/api/v1/music/{kind}/{id}/artworks/{slot}",
            axum::routing::post(upload).delete(remove),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            music_artworks::MAX_BYTES + 256 * 1024,
        ))
}
pub fn public_router() -> Router<AppState> {
    Router::new().route("/music/artwork/{id}", get(serve))
}
fn response(value: serde_json::Value) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        Json(value),
    )
        .into_response()
}
async fn list(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    current.require_scope("read:statuses")?;
    let items =
        music_artworks::for_target(&state, Target::parse(&kind)?, id, Some(current.account.id))
            .await?;
    Ok(response(
        json!({"items":items.iter().map(music_artworks::Artwork::entity).collect::<Vec<_>>()}),
    ))
}
pub(crate) async fn form(
    mut form: Multipart,
) -> Result<(Vec<u8>, String, Option<String>), ApiError> {
    let mut file = None;
    let mut revision = None;
    let mut csrf = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::BadRequest("Invalid artwork form".into()))?
    {
        match field.name() {
            Some("file") => {
                if file.is_some() {
                    return Err(ApiError::Unprocessable("Choose one artwork file".into()));
                }
                file = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|_| ApiError::BadRequest("Invalid artwork file".into()))?
                        .to_vec(),
                );
            }
            Some("revision") => {
                revision = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("Invalid revision".into()))?,
                );
            }
            Some("csrf") => {
                csrf = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| ApiError::BadRequest("Invalid form token".into()))?,
                );
            }
            _ => {}
        }
    }
    Ok((
        file.ok_or_else(|| ApiError::Unprocessable("Choose an artwork file".into()))?,
        revision.ok_or_else(|| ApiError::Unprocessable("Music revision is required".into()))?,
        csrf,
    ))
}
async fn upload(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((kind, id, slot)): Path<(String, i64, String)>,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let (bytes, revision, _) = form(multipart).await?;
    Ok(response(
        music_artworks::upload(
            &state,
            current.account.id,
            Target::parse(&kind)?,
            id,
            &slot,
            &revision,
            bytes,
        )
        .await?
        .entity(),
    ))
}
#[derive(Deserialize)]
struct Remove {
    revision: String,
}
async fn remove(
    State(state): State<AppState>,
    current: CurrentUser,
    Path((kind, id, slot)): Path<(String, i64, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    current.require_scope("write:media")?;
    let input: Remove = super::params::parse_body(&headers, &body)?;
    music_artworks::remove(
        &state,
        current.account.id,
        Target::parse(&kind)?,
        id,
        &slot,
        &input.revision,
    )
    .await?;
    Ok(response(json!({})))
}
async fn serve(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    MaybeUser(token): MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Response {
    let result = async {
        if let Some(token) = &token {
            token.require_scope("read:statuses")?;
        }
        let viewer = token
            .as_ref()
            .map(|u| u.account.id)
            .or_else(|| web.as_ref().map(|u| u.current.account.id));
        let artwork = music_artworks::get(&state, id, viewer).await?;
        let response = super::media::stream_stored_file(
            &state,
            &artwork.file_name,
            &artwork.content_type,
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
                scope: None,
                file: artwork.file_name,
            }),
            credential,
        )
        .await
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
