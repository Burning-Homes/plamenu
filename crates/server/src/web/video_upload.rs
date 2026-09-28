//! Bounded, offset-checked browser upload enhancement. The ordinary multipart
//! POST remains the full no-JavaScript path. Each PATCH holds the draft row
//! lock only while syncing one small chunk and committing its new offset.
use axum::{
    Json,
    body::Bytes,
    extract::{Multipart, Path, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use plamenu_db::video_draft;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::{
    session::{WebUser, csrf_rejection},
    videos::{self, Draft},
};
use crate::{AppState, error::ApiError};

const CHUNK_BYTES: usize = 8 * 1024 * 1024;
const PROBE_BYTES: usize = 64 * 1024;

fn private(result: Result<Response, ApiError>) -> Response {
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Cookie"));
    response
}

fn csrf_header(user: &WebUser, headers: &HeaderMap) -> bool {
    headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| user.csrf_ok(value))
}

/// Creates a private, quota-reserved receiving draft without sending the
/// large File. The browser can rediscover it in Recent drafts after a crash.
pub async fn initiate(
    State(state): State<AppState>,
    user: WebUser,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    private(initiate_inner(&state, &user, &headers, &mut multipart).await)
}

async fn initiate_inner(
    state: &AppState,
    user: &WebUser,
    headers: &HeaderMap,
    multipart: &mut Multipart,
) -> Result<Response, ApiError> {
    let mut draft = Draft::default();
    let mut csrf = String::new();
    let mut bytes = None;
    let mut fingerprint = String::new();
    let mut captions = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::PayloadTooLarge)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            return Err(ApiError::BadRequest(
                "Send the video in bounded chunks".into(),
            ));
        }
        let limit = if name == "captions" {
            super::actions::MAX_CAPTION_BYTES
        } else {
            64 * 1024
        };
        let value = videos::bounded_field(field, limit).await?;
        if name == "captions" {
            if !value.is_empty() {
                captions = super::actions::parse_caption_vtt(&value)?;
            }
            continue;
        }
        let value = String::from_utf8(value)
            .map_err(|_| ApiError::BadRequest("Invalid text encoding".into()))?;
        match name.as_str() {
            "csrf" => csrf = value,
            "channel" => draft.channel = value,
            "title" => draft.title = value,
            "description" => draft.description = value,
            "language" => draft.language = value,
            "visibility" => draft.visibility = value,
            "category" => draft.category = value,
            "licence" => draft.licence = value,
            "spoiler_text" => draft.spoiler_text = value,
            "sensitive" => draft.sensitive = crate::routes::params::truthy(Some(&value)),
            "poster_time" => draft.poster_time = value,
            "alt" => draft.alt = value,
            "transcript" => draft.transcript = value,
            "source_size" => bytes = value.parse::<u64>().ok(),
            "source_fingerprint" => fingerprint = value,
            _ => {}
        }
    }
    if !user.csrf_ok(&csrf) {
        return Ok(csrf_rejection());
    }
    let bytes = bytes.ok_or_else(|| ApiError::BadRequest("Missing video size".into()))?;
    let id = videos::reserve_upload(
        state,
        user,
        headers,
        &draft,
        Some(bytes),
        Some(&fingerprint),
        captions.as_deref(),
    )
    .await?;
    // Snowflake ids exceed JavaScript's safe integer range. Keep identities
    // as decimal strings in every browser-facing JSON response.
    Ok(
        Json(serde_json::json!({"id": id.to_string(), "offset": 0, "total": bytes}))
            .into_response(),
    )
}

async fn owned_receiving(
    state: &AppState,
    id: i64,
    owner: i64,
) -> Result<video_draft::Draft, ApiError> {
    let draft = video_draft::find_owned(&state.pool, id, owner)
        .await?
        .ok_or(ApiError::NotFound)?;
    if draft.source_fingerprint.is_none() || draft.state != "receiving" {
        return Err(ApiError::Conflict(
            "This draft is no longer receiving chunks".into(),
        ));
    }
    if draft.expires_at <= time::OffsetDateTime::now_utc() {
        return Err(ApiError::Conflict("This draft has expired".into()));
    }
    Ok(draft)
}

/// Owner-only progress, including after browser or server restart.
pub async fn status(State(state): State<AppState>, Path(id): Path<i64>, user: WebUser) -> Response {
    private(
        async {
            let draft = owned_receiving(&state, id, user.current.account.id).await?;
            Ok(Json(serde_json::json!({
                "id": id.to_string(),
                "offset": draft.source_bytes,
                "total": draft.reserved_bytes,
                "fingerprint": draft.source_fingerprint,
            }))
            .into_response())
        }
        .await,
    )
}

/// Idempotent at the acknowledged offset: clients query status after an
/// uncertain response and send the next chunk from exactly that offset.
pub async fn chunk(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    private(chunk_inner(&state, id, &user, &headers, body).await)
}

async fn chunk_inner(
    state: &AppState,
    id: i64,
    user: &WebUser,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !csrf_header(user, headers) {
        return Ok(csrf_rejection());
    }
    if body.is_empty() || body.len() > CHUNK_BYTES {
        return Err(ApiError::PayloadTooLarge);
    }
    let offset = headers
        .get("x-upload-offset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| ApiError::BadRequest("Missing upload offset".into()))?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !video_draft::lock_receiving(&mut tx, id, user.current.account.id).await? {
        return Err(ApiError::NotFound);
    }
    let draft = sqlx::query!(
        "SELECT source_file, source_bytes, reserved_bytes, source_fingerprint
         FROM video_drafts WHERE media_id = $1 AND account_id = $2",
        id,
        user.current.account.id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    if draft.source_fingerprint.is_none() || draft.source_bytes != offset {
        return Err(ApiError::Conflict(
            "Upload offset changed; refresh the draft".into(),
        ));
    }
    let next = offset
        .checked_add(i64::try_from(body.len()).unwrap_or(i64::MAX))
        .ok_or(ApiError::PayloadTooLarge)?;
    if next > draft.reserved_bytes {
        return Err(ApiError::PayloadTooLarge);
    }
    let offset_u64 =
        u64::try_from(offset).map_err(|_| ApiError::BadRequest("Invalid upload offset".into()))?;
    state
        .media
        .append_synced(&draft.source_file, offset_u64, body.to_vec())
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if !video_draft::advance_receiving(&mut tx, id, user.current.account.id, offset, next).await? {
        return Err(ApiError::Conflict("Upload was cancelled or changed".into()));
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(Json(serde_json::json!({"offset": next, "total": draft.reserved_bytes})).into_response())
}

/// Finalize only the exact, fsynced source length and verify the local file
/// fingerprint before queuing the encoder. A client-reported count is never
/// accepted as completion evidence.
pub async fn finish(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: WebUser,
    headers: HeaderMap,
) -> Response {
    private(finish_inner(&state, id, &user, &headers).await)
}

async fn finish_inner(
    state: &AppState,
    id: i64,
    user: &WebUser,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    if !csrf_header(user, headers) {
        return Ok(csrf_rejection());
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(plamenu_db::DbError::from)?;
    if !video_draft::lock_receiving(&mut tx, id, user.current.account.id).await? {
        return Err(ApiError::NotFound);
    }
    let draft = sqlx::query!(
        "SELECT source_file, source_bytes, reserved_bytes, source_fingerprint
         FROM video_drafts WHERE media_id = $1 AND account_id = $2",
        id,
        user.current.account.id,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(plamenu_db::DbError::from)?;
    let expected = draft.source_fingerprint.ok_or(ApiError::NotFound)?;
    if draft.source_bytes != draft.reserved_bytes || draft.source_bytes <= 0 {
        return Err(ApiError::Conflict("Upload is incomplete".into()));
    }
    let source_bytes = u64::try_from(draft.source_bytes).expect("positive source length");
    state
        .media
        .append_synced(&draft.source_file, source_bytes, Vec::new())
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut stored = state
        .media
        .open(&draft.source_file)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if stored.len != source_bytes {
        return Err(ApiError::Conflict("Stored source length changed".into()));
    }
    let probe_len = usize::try_from(stored.len.min(PROBE_BYTES as u64))
        .expect("probe length bounded by 64 KiB");
    let mut probe = vec![0; probe_len * 2];
    stored
        .reader
        .read_exact(&mut probe[..probe_len])
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    stored
        .reader
        .seek(std::io::SeekFrom::Start(stored.len - probe_len as u64))
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    stored
        .reader
        .read_exact(&mut probe[probe_len..])
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut actual = String::with_capacity(64);
    for byte in Sha256::digest(&probe) {
        write!(&mut actual, "{byte:02x}").expect("writing to String cannot fail");
    }
    if actual != expected {
        return Err(ApiError::Conflict(
            "Selected file does not match this draft".into(),
        ));
    }
    if !video_draft::finish_receiving(&mut tx, id, user.current.account.id, draft.source_bytes)
        .await?
    {
        return Err(ApiError::Conflict("Upload was cancelled or changed".into()));
    }
    tx.commit().await.map_err(plamenu_db::DbError::from)?;
    Ok(Json(serde_json::json!({"id": id.to_string(), "phase": "queued"})).into_response())
}
