//! Media upload (`/api/v{1,2}/media`), management (`GET`/`PUT`/`DELETE
//! /api/v1/media/{id}`) and serving (`/media/{file}`).

use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use plamenu_db::{account, account_media, id, media, video_captions};
use serde::Deserialize;
use serde_json::{Value, json};

use super::params::parse_body;
use crate::auth::CurrentUser;
use crate::entities::media_json;
use crate::error::ApiError;
use crate::media_processing::{
    FullMedia, MAX_AV_UPLOAD_BYTES, MAX_UPLOAD_BYTES, PreviewMedia, is_still_image,
    process_upload_blocking,
};
use crate::media_transcode::AvKind;
use crate::media_worker::store_av_output;
use crate::state::AppState;

/// Mastodon's wording when transcoding failed (`processing_error`).
const PROCESSING_ERROR: &str = "Error processing thumbnail for uploaded media";

/// Parses a `focus` parameter — Mastodon's `"x,y"` form (unparsable values
/// fall back to 0.0, like Ruby's `to_f`).
pub(crate) fn parse_focus(raw: &str) -> (f64, f64) {
    let mut parts = raw.split(',');
    let x = parts
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0);
    let y = parts
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0);
    (x, y)
}

struct UploadForm {
    file: Option<Vec<u8>>,
    description: Option<String>,
    focus: Option<(f64, f64)>,
}

async fn read_upload_form(mut multipart: Multipart) -> Result<UploadForm, ApiError> {
    let mut form = UploadForm {
        file: None,
        description: None,
        focus: None,
    };
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("invalid multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("upload failed: {e}")))?;
                if bytes.len() > MAX_AV_UPLOAD_BYTES {
                    return Err(ApiError::Unprocessable(
                        "Validation failed: File size exceeds the limit".into(),
                    ));
                }
                form.file = Some(bytes.to_vec());
            }
            "description" => {
                form.description = Some(field.text().await.unwrap_or_default());
            }
            "focus" => {
                let raw = field.text().await.unwrap_or_default();
                if !raw.trim().is_empty() {
                    form.focus = Some(parse_focus(&raw));
                }
            }
            _ => {}
        }
    }
    Ok(form)
}

/// Processes an image upload in-request and stores the finished row.
async fn create_image(
    state: &AppState,
    account_id: i64,
    file: Vec<u8>,
    form: &UploadForm,
) -> Result<Value, ApiError> {
    let settings = state.settings_cache.get(&state.pool).await?;
    let full = FullMedia::from_setting(&settings.media_full_processing);
    let preview = PreviewMedia::from_setting(&settings.media_preview_processing);
    let params =
        crate::media_processing::EncodeParams::from_settings(&settings, &state.config.ffmpeg_path);
    let processed = process_upload_blocking(file, full, preview, params).await?;
    let media_id = id::next();
    let stem = crate::media_processing::storage_stem(media_id);
    let file_name = format!("{stem}.{}", processed.extension);
    let file_size = i64::try_from(processed.bytes.len()).unwrap_or(i64::MAX);
    state
        .media
        .put(&file_name, processed.bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut thumbnail_file_size = None;
    let small = match processed.small {
        Some(small) => {
            let small_name = format!("{stem}.small.{}", small.extension);
            thumbnail_file_size = Some(i64::try_from(small.bytes.len()).unwrap_or(i64::MAX));
            state
                .media
                .put(&small_name, small.bytes)
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Some((small_name, small.width, small.height))
        }
        None => None,
    };
    let stored = media::create_local(
        &state.pool,
        media::NewLocalMedia {
            description: form.description.as_deref().filter(|d| !d.is_empty()),
            width: Some(i32::try_from(processed.width).unwrap_or(i32::MAX)),
            height: Some(i32::try_from(processed.height).unwrap_or(i32::MAX)),
            blurhash: processed.blurhash.as_deref(),
            focus: form.focus,
            small: small
                .as_ref()
                .map(|(name, width, height)| media::SmallStyle {
                    file_name: name,
                    width: i32::try_from(*width).unwrap_or(i32::MAX),
                    height: i32::try_from(*height).unwrap_or(i32::MAX),
                }),
            ..media::NewLocalMedia::new(account_id, media_id, &file_name, processed.content_type)
        },
    )
    .await?;
    media::set_file_sizes(&state.pool, media_id, file_size, thumbnail_file_size)
        .await
        .ok();
    Ok(media_json(&state.config.domain, &stored, false))
}

/// Transcodes a video/audio upload in-request (the v1 path — Mastodon only
/// defers processing on v2).
async fn create_av_sync(
    state: &AppState,
    account_id: i64,
    file: &[u8],
    form: &UploadForm,
    params: &crate::media_transcode::TranscodeParams,
) -> Result<Value, ApiError> {
    let processed = crate::media_transcode::process_av(&state.config, file, params).await?;
    store_processed_av(state, account_id, processed, form).await
}

async fn store_processed_av(
    state: &AppState,
    account_id: i64,
    processed: crate::media_transcode::ProcessedAv,
    form: &UploadForm,
) -> Result<Value, ApiError> {
    let media_id = id::next();
    let out = store_av_output(state, media_id, &processed).await?;
    let stored = media::create_local(
        &state.pool,
        media::NewLocalMedia {
            description: form.description.as_deref().filter(|d| !d.is_empty()),
            kind: processed.kind,
            width: processed.width,
            height: processed.height,
            blurhash: processed.blurhash.as_deref(),
            focus: form.focus,
            small: out
                .small
                .as_ref()
                .map(|(name, width, height)| media::SmallStyle {
                    file_name: name,
                    width: *width,
                    height: *height,
                }),
            duration: processed.duration,
            frame_rate: processed.frame_rate.as_deref(),
            bitrate: processed.bitrate,
            ..media::NewLocalMedia::new(
                account_id,
                media_id,
                &out.file_name,
                processed.content_type,
            )
        },
    )
    .await?;
    media::set_file_sizes(
        &state.pool,
        media_id,
        out.file_size,
        out.thumbnail_file_size,
    )
    .await
    .ok();
    Ok(media_json(&state.config.domain, &stored, false))
}

/// Spools a video/audio upload and queues it for the transcoding worker
/// (the v2 path; the response is a 202 with a `url`-less entity).
async fn create_av_queued(
    state: &AppState,
    account_id: i64,
    kind: AvKind,
    file: Vec<u8>,
    form: &UploadForm,
) -> Result<Value, ApiError> {
    let media_id = id::next();
    let orig_name = format!("{}.orig", crate::media_processing::storage_stem(media_id));
    state
        .media
        .put(&orig_name, file)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let stored = media::create_queued(
        &state.pool,
        media::NewQueuedMedia {
            account_id,
            media_id,
            file_name: &orig_name,
            content_type: match kind {
                AvKind::Audio => "audio/mpeg",
                AvKind::Video | AvKind::Gifv => "video/mp4",
            },
            kind: match kind {
                AvKind::Audio => "audio",
                AvKind::Gifv => "gifv",
                AvKind::Video => "video",
            },
            description: form.description.as_deref().filter(|d| !d.is_empty()),
            focus: form.focus,
        },
    )
    .await?;
    Ok(media_json(&state.config.domain, &stored, false))
}

/// Stores one uploaded file — an image in-request, or audio/video via ffmpeg
/// (synchronously, or spooled for the worker when `delay_processing`) — and
/// returns the status code Mastodon answers with plus the media entity. Shared
/// by the REST upload endpoints and the web composer.
pub(crate) async fn store_upload(
    state: &AppState,
    account_id: i64,
    file: Vec<u8>,
    description: Option<String>,
    focus: Option<(f64, f64)>,
    delay_processing: bool,
) -> Result<(StatusCode, Value), ApiError> {
    let form = UploadForm {
        file: None,
        description,
        focus,
    };

    // Images (and still GIFs) always process in-request; animated GIFs and
    // anything that is not an image go through ffmpeg.
    if is_still_image(&file) {
        if file.len() > MAX_UPLOAD_BYTES {
            return Err(ApiError::Unprocessable(
                "Validation failed: File size exceeds the limit".into(),
            ));
        }
        let entity = create_image(state, account_id, file, &form).await?;
        return Ok((StatusCode::OK, entity));
    }

    let settings = state.settings_cache.get(&state.pool).await?;
    // An animated GIF stays a GIF when the operator says so — or when it
    // carries transparency, which the gifv conversion (H.264, yuv420p) cannot:
    // it would render the transparent regions as a solid background. GIF is
    // universally displayed, so keeping it is always federation-safe.
    if crate::media_processing::is_animated_gif(&file)
        && (settings.media_local_gif_handling == "keep"
            || crate::media_processing::gif_first_frame_has_alpha(&file))
    {
        if file.len() > MAX_UPLOAD_BYTES {
            return Err(ApiError::Unprocessable(
                "Validation failed: File size exceeds the limit".into(),
            ));
        }
        // The animated guard in the image pipeline passes the GIF through
        // whatever the full-processing mode is; the row's kind stays "image"
        // and browsers animate it natively.
        let entity = create_image(state, account_id, file, &form).await?;
        return Ok((StatusCode::OK, entity));
    }

    // Probe + validate up front so a doomed upload fails the request, not
    // the background job.
    let params = crate::media_transcode::TranscodeParams::from_settings(&settings);
    let kind = crate::media_transcode::classify_upload(&state.config, &file, &params).await?;
    if delay_processing {
        let entity = create_av_queued(state, account_id, kind, file, &form).await?;
        return Ok((StatusCode::ACCEPTED, entity));
    }
    let entity = create_av_sync(state, account_id, &file, &form, &params).await?;
    Ok((StatusCode::OK, entity))
}

async fn upload(
    state: AppState,
    current: CurrentUser,
    multipart: Multipart,
    delay_processing: bool,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    current.require_scope("write:media")?;
    let mut form = read_upload_form(multipart).await?;
    let file = form
        .file
        .take()
        .ok_or_else(|| ApiError::Unprocessable("Validation failed: File can't be blank".into()))?;
    let (code, entity) = store_upload(
        &state,
        current.account.id,
        file,
        form.description,
        form.focus,
        delay_processing,
    )
    .await?;
    Ok((code, Json(entity)))
}

/// `POST /api/v1/media` — synchronous, even for video/audio.
pub async fn upload_v1(
    State(state): State<AppState>,
    current: CurrentUser,
    multipart: Multipart,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    upload(state, current, multipart, false).await
}

/// `POST /api/v2/media` — video/audio answer 202 and transcode in the
/// background; images still finish in-request (Mastodon's
/// `delay_processing` only covers the larger formats).
pub async fn upload_v2(
    State(state): State<AppState>,
    current: CurrentUser,
    multipart: Multipart,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    upload(state, current, multipart, true).await
}

/// Loads an owned, unattached upload (Mastodon's `set_media_attachment`
/// scope) and applies `check_processing`.
async fn owned_unattached(
    state: &AppState,
    current: &CurrentUser,
    media_id: i64,
) -> Result<media::Media, ApiError> {
    let item = media::find_owned(&state.pool, media_id, current.account.id)
        .await?
        .filter(|m| m.status_id.is_none())
        .ok_or(ApiError::NotFound)?;
    if item.processing == "failed" {
        return Err(ApiError::Unprocessable(PROCESSING_ERROR.into()));
    }
    Ok(item)
}

fn status_for(item: &media::Media) -> StatusCode {
    if item.not_processed() {
        // 206: still processing — poll again, like Mastodon.
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    }
}

/// `GET /api/v1/media/{id}` — the polling endpoint for async (v2) uploads.
pub async fn show(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(media_id): Path<i64>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    current.require_scope("write:media")?;
    let item = owned_unattached(&state, &current, media_id).await?;
    Ok((
        status_for(&item),
        Json(media_json(&state.config.domain, &item, false)),
    ))
}

#[derive(Deserialize)]
pub struct UpdateParams {
    pub description: Option<String>,
    pub focus: Option<String>,
}

/// `PUT /api/v1/media/{id}` — update description (alt text) and focal
/// point; like Mastodon, only unattached uploads can be edited.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(media_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    current.require_scope("write:media")?;
    let params: UpdateParams = parse_body(&headers, &body)?;
    owned_unattached(&state, &current, media_id).await?;
    let focus = params
        .focus
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
        .map(parse_focus);
    let updated = media::update_attributes(
        &state.pool,
        media_id,
        current.account.id,
        params.description.as_ref().map(|d| {
            let trimmed: Option<&str> = Some(d.as_str()).filter(|d| !d.is_empty());
            trimmed
        }),
        focus,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok((
        status_for(&updated),
        Json(media_json(&state.config.domain, &updated, false)),
    ))
}

/// `DELETE /api/v1/media/{id}` — removes an unattached upload; attached
/// media answer Mastodon's in-use 422.
pub async fn destroy(
    State(state): State<AppState>,
    current: CurrentUser,
    Path(media_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    current.require_scope("write:media")?;
    let item = media::find_owned(&state.pool, media_id, current.account.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.status_id.is_some() {
        return Err(ApiError::Unprocessable(
            "Media attachment is currently used by a status".into(),
        ));
    }
    media::delete_unattached(&state.pool, media_id, current.account.id).await?;
    for file in [item.file_name.as_deref(), item.small_file_name.as_deref()]
        .into_iter()
        .flatten()
    {
        let _ = state.media.delete(file).await;
    }
    Ok(Json(json!({})))
}

/// One parsed `Range: bytes=…` request against a file of known length.
pub(crate) enum ByteRange {
    /// No (or an ignorable) range: serve the whole file with a 200.
    Full,
    /// A satisfiable single range: serve a 206 of `[start, end]` (inclusive).
    Partial(u64, u64),
    /// Syntactically a range but nothing in it is satisfiable: 416.
    Unsatisfiable,
}

/// Parses a `Range` header for a `total`-byte file. Only single byte ranges
/// are honored; multipart ranges (rare, and useless for media scrubbing) and
/// malformed values fall back to the full body, as RFC 9110 permits.
pub(crate) fn parse_range(header: Option<&axum::http::HeaderValue>, total: u64) -> ByteRange {
    let Some(raw) = header.and_then(|value| value.to_str().ok()) else {
        return ByteRange::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    if spec.contains(',') {
        return ByteRange::Full;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    let (start, end) = match (start.trim(), end.trim()) {
        // bytes=-N — the final N bytes.
        ("", suffix) => {
            let Ok(suffix) = suffix.parse::<u64>() else {
                return ByteRange::Full;
            };
            if suffix == 0 || total == 0 {
                return ByteRange::Unsatisfiable;
            }
            (total.saturating_sub(suffix), total - 1)
        }
        // bytes=N- — from N to the end.
        (start, "") => {
            let Ok(start) = start.parse::<u64>() else {
                return ByteRange::Full;
            };
            if start >= total {
                return ByteRange::Unsatisfiable;
            }
            (start, total - 1)
        }
        (start, end) => {
            let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                return ByteRange::Full;
            };
            if start > end || start >= total {
                return ByteRange::Unsatisfiable;
            }
            (start, end.min(total - 1))
        }
    };
    ByteRange::Partial(start, end)
}

/// A streamed body over `len` bytes of an opened media file (constant
/// memory — a feature video must never be buffered per request).
fn stream_body(reader: Box<dyn crate::storage::MediaReader>, len: u64) -> axum::body::Body {
    use futures_util::TryStreamExt as _;
    use tokio::io::AsyncReadExt;
    // Tokio disk reads use the blocking pool. Small default chunks repeatedly
    // wake it and emit HTTP frames for a single large video; keep reads bounded
    // at 64 KiB, as in the other large-file paths, without reading ahead.
    let stream =
        tokio_util::io::ReaderStream::with_capacity(reader.take(len), 64 * 1024).map_err(|error| {
            // A cancelled body is dropped without a read error. Actual storage
            // failures need evidence, but error strings may include private
            // backend paths or URLs, so record only the I/O error class.
            tracing::warn!(error_kind = ?error.kind(), "stored media read failed");
            error
        });
    axum::body::Body::from_stream(stream)
}

/// `GET /media/{file_name}` — serves stored uploads, streaming from the
/// store with single-range `Range` support (206). Seeking in the built-in
/// player, mobile Chrome playback and third-party clients all require
/// byte ranges; buffering whole files would sink the host on feature video.
/// Names are server-generated (`{snowflake}.{ext}`); anything else is a 404.
pub async fn serve(
    State(state): State<AppState>,
    Path(file_name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !file_name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        || file_name.starts_with('.')
        // Spooled originals awaiting transcoding are never public.
        || file_name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("orig"))
    {
        return Err(ApiError::NotFound);
    }
    if !media::public_file_allowed(&state.pool, &file_name).await? {
        return Err(ApiError::NotFound);
    }
    // Content is immutable: the file name embeds a unique id.
    let content_type = crate::media_processing::content_type_for(&file_name);
    if file_name.contains(".draft.progressive") {
        return serve_progressive(&state, &file_name, &headers).await;
    }
    stream_stored_file(
        &state,
        &file_name,
        content_type,
        "public, max-age=31536000, immutable",
        &headers,
    )
    .await
}

async fn serve_progressive(
    state: &AppState,
    file: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let data = sqlx::query_as::<_, plamenu_db::native_video_hls::Progressive>("SELECT p.* FROM native_video_progressive_files p JOIN native_video_hls_renditions r ON r.media_id=p.media_id AND r.height=p.height AND r.generation=p.generation JOIN video_drafts d ON d.media_id=r.media_id AND d.generation=r.generation AND d.state='ready' JOIN video_draft_outputs o ON o.file_name=p.file_name AND o.media_id=r.media_id AND o.generation=r.generation AND NOT o.retired WHERE p.file_name=$1")
        .bind(file).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let p = data
        .filter(plamenu_db::native_video_hls::Progressive::valid)
        .ok_or(ApiError::NotFound)?;
    let opened = state
        .media
        .open(file)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if i64::try_from(opened.len).ok() != Some(p.size_bytes) {
        return Err(ApiError::NotFound);
    }
    drop(opened);
    let etag = format!("\"{}\"", p.sha256);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| {
            v.split(',').any(|tag| {
                tag.trim() == etag
                    || tag.trim() == "*"
                    || tag.trim().strip_prefix("W/") == Some(etag.as_str())
            })
        })
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".into(),
                ),
            ],
        )
            .into_response());
    }
    let mut request = headers.clone();
    if request
        .get(header::IF_RANGE)
        .is_some_and(|h| h.to_str().ok() != Some(etag.as_str()))
    {
        request.remove(header::RANGE);
    }
    let mut response = stream_stored_file(
        state,
        file,
        "video/mp4",
        "public, max-age=31536000, immutable",
        &request,
    )
    .await?;
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().map_err(|_| ApiError::NotFound)?);
    Ok(response)
}

/// Explicit native MP4 download offer; anonymous audience checks still apply.
pub async fn download(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let result=async{
        let file:Option<String>=sqlx::query_scalar("SELECT m.file_name
            FROM video_publications v
            JOIN media_attachments m ON m.id=v.media_id AND m.status_id=v.status_id
            JOIN statuses s ON s.id=v.status_id
            JOIN accounts a ON a.id=s.account_id
            JOIN video_channels c ON c.account_id=v.channel_id
            JOIN accounts o ON o.id=c.owner_account_id
            LEFT JOIN video_distribution p ON p.media_id=m.id
            WHERE v.media_id=$1 AND COALESCE(p.download_enabled,true)
                AND s.deleted_at IS NULL AND s.visibility IN ('public','unlisted')
                AND a.suspended_at IS NULL AND o.suspended_at IS NULL
                AND m.processing='complete' AND m.content_type='video/mp4' AND m.file_name IS NOT NULL\n-- STUBFILTER: exclude deleted statuses directly or through public_native_videos")
            .bind(id).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?;
        let file=file.ok_or(ApiError::NotFound)?;
        if !media::public_file_allowed(&state.pool,&file).await?{return Err(ApiError::NotFound);}
        let mut response=stream_stored_file(&state,&file,"video/mp4","private, no-store",&headers).await?;
        response.headers_mut().insert(header::CONTENT_DISPOSITION,format!("attachment; filename=\"video-{id}.mp4\"").parse().unwrap());
        Ok::<_,ApiError>(response)
    }.await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    response
}

/// `GET /media/{id}/captions.vtt` — mutable, author-supplied timed captions.
/// Caption URLs are same-origin so native players do not need cross-origin
/// track credentials. Unlike immutable media bytes, edits may replace the
/// cues, hence `no-cache` rather than a year-long immutable policy.
pub async fn captions(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(query): Query<CaptionRevision>,
) -> Result<Response, ApiError> {
    let body = media::public_caption_vtt(&state.pool, media_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    check_caption_revision(query.v.as_deref(), &body)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response())
}

/// Proxy a remote language-specific `WebVTT` track on demand. Signed AP URLs
/// are origin-fenced at ingest; the fetcher enforces its own SSRF protections
/// and byte ceiling. Browsers see only this same-origin, non-cacheable URL.
pub async fn language_caption(
    State(state): State<AppState>,
    Path((media_id, name)): Path<(i64, String)>,
    Query(query): Query<CaptionRevision>,
) -> Result<Response, ApiError> {
    let language = name.strip_suffix(".vtt").ok_or(ApiError::NotFound)?;
    let (body, remote) = public_language_caption_text(&state, media_id, language).await?;
    if !remote {
        check_caption_revision(query.v.as_deref(), &body)?;
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (
                header::CACHE_CONTROL,
                if remote { "no-store" } else { "no-cache" },
            ),
        ],
        body,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct CaptionRevision {
    v: Option<String>,
}

fn check_caption_revision(revision: Option<&str>, body: &str) -> Result<(), ApiError> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    if let Some(revision) = revision {
        let hash = Sha256::digest(body.as_bytes());
        if revision != base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash[..12]) {
            return Err(ApiError::NotFound);
        }
    }
    Ok(())
}

/// Shared by the raw track and the opt-in, server-rendered text transcript.
/// The latter must apply precisely the same visibility and origin guards.
pub(crate) async fn public_language_caption_text(
    state: &AppState,
    media_id: i64,
    language: &str,
) -> Result<(String, bool), ApiError> {
    let source = video_captions::public_source(&state.pool, media_id, language)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(body) = source.vtt_body {
        return Ok((body, false));
    }
    let origin = source.remote_url.ok_or(ApiError::NotFound)?;
    let fetched = state
        .federation
        .fetch_media_limited(&origin, crate::web::actions::MAX_CAPTION_BYTES as u64)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let original = url::Url::parse(&origin).map_err(|_| ApiError::NotFound)?;
    let final_url = url::Url::parse(&fetched.final_url).map_err(|_| ApiError::NotFound)?;
    if original.scheme() != final_url.scheme()
        || original.host_str() != final_url.host_str()
        || original.port_or_known_default() != final_url.port_or_known_default()
    {
        return Err(ApiError::NotFound);
    }
    let body = crate::web::actions::parse_caption_vtt(&fetched.bytes)
        .map_err(|_| ApiError::NotFound)?
        .ok_or(ApiError::NotFound)?;
    Ok((body, true))
}

/// Streams a stored file with single-range `Range` support (206) and constant
/// memory. Shared by `/media/{file}` and the HLS segment cache; the caller
/// supplies the content type and cache policy (uploads and cached HLS segments
/// are both immutable). `Access-Control-Allow-Origin` comes from the media
/// router's CORS layer, not here.
pub(crate) async fn stream_stored_file(
    state: &AppState,
    file_name: &str,
    content_type: &str,
    cache_control: &'static str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let opened = state
        .media
        .open(file_name)
        .await
        .map_err(|_| ApiError::NotFound)?;
    stream_open_file(opened, content_type, cache_control, headers).await
}
pub(crate) async fn stream_open_file(
    opened: crate::storage::MediaRead,
    content_type: &str,
    cache_control: &'static str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let common = [
        (header::CONTENT_TYPE, content_type.to_owned()),
        (header::CACHE_CONTROL, cache_control.to_owned()),
        (header::ACCEPT_RANGES, "bytes".to_owned()),
    ];
    match parse_range(headers.get(header::RANGE), opened.len) {
        ByteRange::Full => {
            let len = opened.len;
            Ok((
                common,
                [(header::CONTENT_LENGTH, len.to_string())],
                stream_body(opened.reader, len),
            )
                .into_response())
        }
        ByteRange::Partial(start, end) => {
            use tokio::io::AsyncSeekExt;
            let mut reader = opened.reader;
            reader
                .seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            let len = end - start + 1;
            Ok((
                StatusCode::PARTIAL_CONTENT,
                common,
                [
                    (header::CONTENT_LENGTH, len.to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{}", opened.len),
                    ),
                ],
                stream_body(reader, len),
            )
                .into_response())
        }
        ByteRange::Unsatisfiable => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            common,
            [(header::CONTENT_RANGE, format!("bytes */{}", opened.len))],
        )
            .into_response()),
    }
}

/// How long a single proxy request will wait on an on-demand download before
/// giving up (and either 404ing or, when opted in, redirecting to the origin).
const PROXY_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The `?d=1` marker: the viewing user opted into direct remote fetch as a
/// last resort. Baked into the serialized proxy URL from that user's setting,
/// so it travels to unauthenticated `<img>` loads of third-party clients too.
#[derive(Deserialize)]
pub struct ProxyQuery {
    #[serde(default)]
    d: Option<String>,
}

impl ProxyQuery {
    fn allow_direct(&self) -> bool {
        self.d.as_deref() == Some("1")
    }
}

/// A 302 to our own immutable `/media/{file}` copy.
fn redirect_local(domain: &str, file: &str) -> Response {
    // Not cached: the file name may change once a fresh copy downloads.
    (
        [(header::CACHE_CONTROL, "no-store")],
        Redirect::temporary(&format!("https://{domain}/media/{file}")),
    )
        .into_response()
}

/// A 302 to the true origin — the ONLY response that sends a client
/// off-instance, reached only with the opt-in `?d=1` marker after we could
/// not cache the file ourselves.
pub(super) fn redirect_origin(url: &str) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Redirect::temporary(url),
    )
        .into_response()
}

/// `GET /media/proxy/{kind}/{id}` — resolves any remote media through the
/// instance so a client never contacts the origin. When we already hold the
/// bytes it 302s to the immutable `/media/{file}`; otherwise it fetches and
/// re-serves on demand (recaching a retention-evicted or never-downloaded
/// file), then 302s. Only when the fetch fails AND the URL carries `?d=1`
/// (the viewer opted in) does it fall back to the origin URL — otherwise it
/// fails closed with a 404 and the client shows its own placeholder.
pub async fn proxy(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
    query: Query<ProxyQuery>,
) -> Result<Response, ApiError> {
    proxy_inner(&state, &kind, id, false, query.allow_direct()).await
}

/// `GET /media/proxy/{kind}/{id}/small` — the preview/thumbnail variant (only
/// meaningful for `attachment`; other kinds ignore it).
pub async fn proxy_small(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
    query: Query<ProxyQuery>,
) -> Result<Response, ApiError> {
    proxy_inner(&state, &kind, id, true, query.allow_direct()).await
}

#[allow(
    clippy::too_many_lines,
    reason = "one media-kind dispatch keeps every proxy lane behind the same suspension and origin fences"
)]
async fn proxy_inner(
    state: &AppState,
    kind: &str,
    id: i64,
    small: bool,
    allow_direct: bool,
) -> Result<Response, ApiError> {
    let domain = &state.config.domain;
    match kind {
        "attachment" => {
            if media::public_access_blocked(&state.pool, id).await? {
                return Err(ApiError::NotFound);
            }
            // Compatibility URLs emitted before the progressive gateway may
            // remain in client caches. Upgrade those requests in place; do
            // not start the legacy detached whole-video download.
            if !small && media::hls_master_url(&state.pool, id).await?.is_some() {
                return Ok((
                    [(header::CACHE_CONTROL, "no-store")],
                    Redirect::temporary(&format!("https://{domain}/media/play/{id}/video.mp4")),
                )
                    .into_response());
            }
            let item = ensure(crate::media_worker::ensure_attachment_for_proxy(
                state, id, small,
            ))
            .await;
            if let Some(item) = item {
                let file = if small {
                    item.small_file_name
                        .as_deref()
                        .or(item.file_name.as_deref())
                } else {
                    item.file_name.as_deref()
                };
                if let Some(file) = file {
                    // A viewer reached the cached copy of remote media:
                    // retention keeps recently-served files. Poster-only
                    // loads (timelines) don't count — only actual media
                    // access should keep a big video hot.
                    if !small && item.remote_url.is_some() {
                        media::touch_served(&state.pool, id).await.ok();
                    }
                    return Ok(redirect_local(domain, file));
                }
                if allow_direct {
                    let origin = if small {
                        item.thumbnail_remote_url
                            .as_deref()
                            .or(item.remote_url.as_deref())
                    } else if item.remote_audio_url.is_none() {
                        item.remote_url.as_deref()
                    } else {
                        // A separated-audio video (PeerTube 6.x HLS) has no
                        // muxed origin file: `remote_url` is the SILENT video
                        // rendition and its audio lives in `remote_audio_url`.
                        // Never send an opted-in viewer to a soundless clip —
                        // fall through to 404 so the client shows a placeholder.
                        None
                    };
                    if let Some(origin) = origin {
                        return Ok(redirect_origin(origin));
                    }
                }
            }
        }
        "avatar" | "header" => {
            if account::is_suspended(&state.pool, id).await? {
                return Err(ApiError::NotFound);
            }
            let which = if kind == "header" {
                account_media::HEADER
            } else {
                account_media::AVATAR
            };
            let account = ensure(crate::media_worker::ensure_account_image_cached(
                state, id, which,
            ))
            .await;
            if let Some(account) = account {
                let (file, origin) = if which == account_media::HEADER {
                    (account.header_file_name, account.header_remote_url)
                } else {
                    (account.avatar_file_name, account.avatar_remote_url)
                };
                if let Some(file) = file {
                    return Ok(redirect_local(domain, &file));
                }
                if allow_direct && let Some(origin) = origin {
                    return Ok(redirect_origin(&origin));
                }
            }
        }
        "emoji" => {
            let emoji = ensure(crate::media_worker::ensure_emoji_cached(state, id)).await;
            if let Some(emoji) = emoji {
                if let Some(file) = emoji.image_file_name {
                    return Ok(redirect_local(domain, &file));
                }
                if allow_direct && let Some(origin) = emoji.image_remote_url {
                    return Ok(redirect_origin(&origin));
                }
            }
        }
        "card" => {
            let card = ensure(crate::media_worker::ensure_card_cached(state, id)).await;
            if let Some(card) = card {
                if let Some(file) = card.image_file_name {
                    return Ok(redirect_local(domain, &file));
                }
                if allow_direct && let Some(origin) = card.image_url {
                    return Ok(redirect_origin(&origin));
                }
            }
        }
        _ => {}
    }
    Err(ApiError::NotFound)
}

/// Runs an on-demand cache future under the proxy timeout, swallowing fetch
/// errors/timeouts into `None` (a failed download is the 404/fallback path,
/// not a 500).
async fn ensure<T>(
    fut: impl std::future::Future<Output = Result<Option<T>, ApiError>>,
) -> Option<T> {
    match tokio::time::timeout(PROXY_FETCH_TIMEOUT, fut).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) | Err(_) => None,
    }
}

#[cfg(test)]
mod streaming_tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use http_body_util::BodyExt;
    use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};

    struct ObservedReader {
        inner: std::io::Cursor<Vec<u8>>,
        fail: bool,
        bytes_read: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    impl AsyncRead for ObservedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.fail {
                return Poll::Ready(Err(std::io::ErrorKind::PermissionDenied.into()));
            }
            let before = buf.filled().len();
            let result = Pin::new(&mut self.inner).poll_read(cx, buf);
            self.bytes_read
                .fetch_add(buf.filled().len() - before, Ordering::Relaxed);
            result
        }
    }

    impl AsyncSeek for ObservedReader {
        fn start_seek(
            mut self: Pin<&mut Self>,
            position: std::io::SeekFrom,
        ) -> std::io::Result<()> {
            Pin::new(&mut self.inner).start_seek(position)
        }

        fn poll_complete(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<u64>> {
            Pin::new(&mut self.inner).poll_complete(cx)
        }
    }

    impl Drop for ObservedReader {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn stored_stream_waits_for_demand_and_releases_reader_on_cancel() {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = ObservedReader {
            inner: std::io::Cursor::new(vec![7; 1024 * 1024]),
            fail: false,
            bytes_read: bytes_read.clone(),
            dropped: dropped.clone(),
        };
        let mut body = super::stream_body(Box::new(reader), 1024 * 1024);
        assert_eq!(bytes_read.load(Ordering::Relaxed), 0);
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(!first.is_empty());
        assert!(first.len() <= 64 * 1024);
        assert_eq!(bytes_read.load(Ordering::Relaxed), first.len());
        tokio::task::yield_now().await;
        assert_eq!(bytes_read.load(Ordering::Relaxed), first.len());
        drop(body);
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(bytes_read.load(Ordering::Relaxed), first.len());
    }

    #[tokio::test]
    async fn stored_stream_batches_large_reads_and_stops_at_requested_length() {
        let length = 4 * 1024 * 1024 + 17;
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = ObservedReader {
            inner: std::io::Cursor::new(vec![7; length + 100]),
            fail: false,
            bytes_read: bytes_read.clone(),
            dropped: dropped.clone(),
        };
        let mut body = super::stream_body(Box::new(reader), length as u64);
        let (mut chunks, mut received) = (0, 0);
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            assert!(data.len() <= 64 * 1024);
            assert!(data.iter().all(|byte| *byte == 7));
            chunks += 1;
            received += data.len();
        }
        assert_eq!(received, length);
        assert_eq!(bytes_read.load(Ordering::Relaxed), length);
        // Catch a return to tiny per-frame disk jobs without requiring one
        // exact chunk shape from every storage backend.
        assert!(chunks <= 128, "large stored file emitted {chunks} chunks");
        drop(body);
        assert!(dropped.load(Ordering::Relaxed));
    }
    #[tokio::test]
    async fn stored_stream_propagates_read_failure_and_releases_reader() {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let reader = ObservedReader {
            inner: std::io::Cursor::new(vec![7; 100]),
            fail: true,
            bytes_read: bytes_read.clone(),
            dropped: dropped.clone(),
        };
        let mut body = super::stream_body(Box::new(reader), 100);
        assert!(body.frame().await.unwrap().is_err());
        assert_eq!(bytes_read.load(Ordering::Relaxed), 0);
        assert!(body.frame().await.is_none());
        drop(body);
        assert!(dropped.load(Ordering::Relaxed));
    }
}
