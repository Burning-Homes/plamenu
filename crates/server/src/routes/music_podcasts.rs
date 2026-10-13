//! Podcast subscriptions, catalogues, and per-listener played state.
use crate::{
    AppState,
    auth::CurrentUser,
    error::ApiError,
    music_podcasts::{self, Query as CatalogueQuery},
};
use axum::{
    Router,
    body::Bytes,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
fn response(value: impl serde::Serialize) -> Response {
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        axum::Json(value),
    )
        .into_response()
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/music/podcasts", get(index).post(subscribe))
        .route("/api/v1/music/podcasts/opml", get(opml))
        .route(
            "/api/v1/music/podcasts/opml/import",
            axum::routing::post(import),
        )
        .route(
            "/api/v1/music/podcasts/opml/upload",
            axum::routing::post(upload),
        )
        .route("/api/v1/music/podcasts/{id}", get(show).delete(unsubscribe))
        .route(
            "/api/v1/music/podcasts/{id}/refresh",
            axum::routing::post(refresh),
        )
        .route("/api/v1/music/podcasts/{id}/episodes", get(episodes))
        .route(
            "/api/v1/music/podcasts/{id}/played",
            axum::routing::put(played_batch),
        )
        .route("/api/v1/music/podcasts/episodes/{id}", get(episode))
        .route(
            "/api/v1/music/podcasts/episodes/{id}/played",
            axum::routing::put(played).delete(unplayed),
        )
}
async fn index(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(query): Query<CatalogueQuery>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    let page = music_podcasts::list(&state, user.account.id, &query).await?;
    Ok(response(json!({"items":page.items,"next":page.next})))
}
#[derive(Deserialize)]
struct Subscribe {
    feed_url: String,
}
async fn subscribe(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    let input: Subscribe = super::params::parse_body(&headers, &body)?;
    let (show, created) =
        music_podcasts::subscribe(&state, user.account.id, &input.feed_url).await?;
    let mut value = music_podcasts::details(&state, show.id, user.account.id).await?;
    value["following"] = json!(true);
    value["created"] = json!(created);
    Ok(response(value))
}
async fn show(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    let value = music_podcasts::details(&state, id, user.account.id).await?;
    Ok(response(value))
}
async fn unsubscribe(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    music_podcasts::unsubscribe(&state, user.account.id, id).await?;
    Ok(response(json!({})))
}
async fn refresh(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    music_podcasts::refresh(&state, id, user.account.id).await?;
    Ok(response(
        music_podcasts::details(&state, id, user.account.id).await?,
    ))
}
#[derive(Default, Deserialize)]
struct EpisodeQuery {
    sort: Option<String>,
    offset: Option<i64>,
}
async fn episodes(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    Query(query): Query<EpisodeQuery>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    let oldest = match query.sort.as_deref() {
        None | Some("newest") => false,
        Some("oldest") => true,
        _ => return Err(ApiError::Unprocessable("Invalid episode order".into())),
    };
    let page = music_podcasts::episodes(&state, id, user.account.id, oldest, query.offset).await?;
    Ok(response(json!({"items":page.items,"next":page.next})))
}
async fn episode(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    Ok(response(
        music_podcasts::episode(&state, id, user.account.id)
            .await?
            .entity(),
    ))
}
async fn played(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    music_podcasts::played(&state, user.account.id, id, true).await?;
    Ok(response(json!({})))
}
async fn unplayed(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    music_podcasts::played(&state, user.account.id, id, false).await?;
    Ok(response(json!({})))
}
async fn opml(State(state): State<AppState>, user: CurrentUser) -> Result<Response, ApiError> {
    user.require_scope("read:statuses")?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/x-opml; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"podcasts.opml\"",
            ),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Authorization, Cookie"),
        ],
        music_podcasts::opml(&state, user.account.id).await?,
    )
        .into_response())
}
async fn import(
    State(state): State<AppState>,
    user: CurrentUser,
    body: Bytes,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    Ok(response(
        music_podcasts::import_opml(&state, user.account.id, &body).await?,
    ))
}

/// Public source audio remains distinct from private per-listener state.
pub fn media_router() -> Router<AppState> {
    Router::new()
        .route("/music/podcasts/episodes/{id}/audio", get(audio))
        .route("/music/podcasts/{kind}/{id}/image", get(image))
}
async fn audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    crate::auth::MaybeUser(token): crate::auth::MaybeUser,
    crate::web::session::MaybeWebUser(web): crate::web::session::MaybeWebUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(user) = &token {
        user.require_scope("read:statuses")?;
    }
    let viewer = token
        .as_ref()
        .map(|u| u.account.id)
        .or_else(|| web.as_ref().map(|u| u.current.account.id));
    if let Some(viewer) = viewer {
        let mut conn = state
            .pool
            .acquire()
            .await
            .map_err(plamenu_db::DbError::from)?;
        crate::music_queues::active_owner(&mut conn, viewer).await?;
    }
    let url: String =
        sqlx::query_scalar("SELECT audio_url FROM music_podcast_episodes WHERE id=$1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(plamenu_db::DbError::from)?
            .ok_or(ApiError::NotFound)?;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.len() < 100
                && v.starts_with("bytes=")
                && !v.contains(',')
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || b"bytes=- ".contains(&b))
        });
    let source = state
        .federation
        .fetch_enclosure(&url, range)
        .await
        .map_err(|_| ApiError::BadGateway("Could not stream podcast audio".into()))?;
    if source.status == 206 {
        let raw = source
            .content_range
            .as_deref()
            .ok_or_else(|| ApiError::BadGateway("Invalid podcast byte range".into()))?;
        let total = raw
            .rsplit('/')
            .next()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| ApiError::BadGateway("Invalid podcast byte range".into()))?;
        if let super::media::ByteRange::Partial(start, end) =
            super::media::parse_range(headers.get(header::RANGE), total)
            && raw != format!("bytes {start}-{end}/{total}")
        {
            return Err(ApiError::BadGateway("Incorrect podcast byte range".into()));
        }
    }
    let mut response = axum::http::Response::builder()
        .status(source.status)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::VARY, "Authorization, Cookie")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_TYPE, &source.content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(range) = &source.content_range {
        response = response.header(header::CONTENT_RANGE, range);
    }
    if source.status == 416 {
        return response
            .body(axum::body::Body::empty())
            .map_err(|e| ApiError::Internal(Box::new(e)));
    }
    if let Some(len) = source.content_length {
        response = response.header(header::CONTENT_LENGTH, len);
    }
    response
        .body(axum::body::Body::from_stream(source.bytes))
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

pub(crate) async fn opml_file(mut form: Multipart, csrf: Option<&str>) -> Result<Bytes, ApiError> {
    let mut file = None;
    let mut token = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::Unprocessable("Invalid OPML upload".into()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "csrf" => {
                let value = field
                    .text()
                    .await
                    .map_err(|_| ApiError::Unprocessable("Invalid form token".into()))?;
                if value.len() > 128 {
                    return Err(ApiError::Forbidden("Invalid form token".into()));
                }
                token = Some(value);
            }
            "file" => {
                if file.is_some() {
                    return Err(ApiError::Unprocessable("Choose one OPML file".into()));
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| ApiError::Unprocessable("Invalid OPML upload".into()))?;
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err(ApiError::PayloadTooLarge);
                }
                file = Some(bytes);
            }
            _ => return Err(ApiError::Unprocessable("Unexpected OPML form field".into())),
        }
    }
    if csrf.is_some() && token.as_deref() != csrf {
        return Err(ApiError::Forbidden("Invalid form token".into()));
    }
    file.ok_or_else(|| ApiError::Unprocessable("Choose an OPML file".into()))
}
async fn upload(
    State(state): State<AppState>,
    user: CurrentUser,
    form: Multipart,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    let bytes = opml_file(form, None).await?;
    Ok(response(
        music_podcasts::import_opml(&state, user.account.id, &bytes).await?,
    ))
}

#[derive(Deserialize)]
struct PlayedBatch {
    episode_ids: Vec<String>,
    played: bool,
}
async fn played_batch(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(show): Path<i64>,
    axum::Json(input): axum::Json<PlayedBatch>,
) -> Result<Response, ApiError> {
    user.require_scope("write:statuses")?;
    let episodes = input
        .episode_ids
        .iter()
        .map(|id| id.parse::<i64>().map_err(|_| ApiError::NotFound))
        .collect::<Result<Vec<_>, _>>()?;
    music_podcasts::played_batch(&state, user.account.id, show, &episodes, input.played).await?;
    Ok(response(json!({})))
}

async fn image(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let url: Option<String> = match kind.as_str() {
        "shows" => sqlx::query_scalar("SELECT image_url FROM music_podcasts WHERE id=$1").bind(id).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?.flatten(),
        "episodes" => sqlx::query_scalar("SELECT COALESCE(e.image_url,p.image_url) FROM music_podcast_episodes e JOIN music_podcasts p ON p.id=e.podcast_id WHERE e.id=$1").bind(id).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?.flatten(),
        _ => return Err(ApiError::NotFound),
    };
    let bytes = state
        .federation
        .fetch_feed_image(&url.ok_or(ApiError::NotFound)?)
        .await
        .map_err(|_| ApiError::BadGateway("Could not load podcast artwork".into()))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(ApiError::PayloadTooLarge);
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let params =
        crate::media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let image = crate::media_processing::process_image_blocking(bytes, 512, params).await?;
    Ok((
        [
            (header::CONTENT_TYPE, image.content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        image.bytes,
    )
        .into_response())
}
