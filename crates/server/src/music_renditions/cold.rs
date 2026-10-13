//! Bounded progressive reads from an owned, lease-fenced encode. Files and
//! authority remain server-side; the HTTP body owns its finite listener lease.
use super::{Format, Listener, Rendition};
use crate::{AppState, error::ApiError};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use tokio::io::AsyncReadExt;
#[derive(Clone)]
pub struct Access {
    pub viewer: Option<i64>,
    pub scope: Option<crate::music_shares::Scope>,
}
impl Access {
    pub(crate) async fn get(
        &self,
        state: &AppState,
        asset: i64,
        job: i64,
    ) -> Result<Rendition, ApiError> {
        let row = if let Some(scope) = &self.scope {
            super::get_shared(state, asset, job, self.viewer, scope).await?
        } else {
            super::get(state, asset, job, self.viewer).await?
        };
        super::authorize_playback(state, &row, self.viewer).await?;
        Ok(row)
    }
}
struct Tail {
    state: AppState,
    access: Access,
    row: Rendition,
    listener: Listener,
    file: tokio::fs::File,
    checked: std::time::Instant,
    started: std::time::Instant,
}
fn io_error(error: ApiError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, error)
}
/// HEAD only describes a ready cache; a probe must not start an encoder.
pub async fn head(
    state: &AppState,
    asset: i64,
    access: Access,
    format: &str,
    bitrate: Option<u32>,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let selection = super::negotiate(state, access.viewer, format, bitrate).await?;
    let source = super::jobs::current_source(
        state,
        asset,
        access.viewer,
        access.scope.as_ref().map(|scope| scope.id),
    )
    .await?;
    let job:Option<i64>=sqlx::query_scalar("SELECT id FROM music_renditions WHERE asset_id=$1 AND source_hash=$2 AND format=$3 AND bitrate=$4 AND state='ready'")
        .bind(asset).bind(source.audio_hash).bind(selection.format.name()).bind(i32::try_from(selection.bitrate).unwrap()).fetch_optional(&state.pool).await.map_err(plamenu_db::DbError::from)?;
    let job = job
        .ok_or_else(|| ApiError::ServiceUnavailable("Playback copy is not prepared yet".into()))?;
    let row = access.get(state, asset, job).await?;
    crate::routes::music_renditions::serve_ready(state, &row, headers, access.clone()).await
}
async fn refresh(tail: &mut Tail) -> Result<(), std::io::Error> {
    let current = tail
        .access
        .get(&tail.state, tail.row.asset_id, tail.row.id)
        .await
        .map_err(io_error)?;
    if current.source_hash != tail.row.source_hash
        || (tail.row.file_name.is_some() && current.file_name != tail.row.file_name)
        || (current.state == "running"
            && (current.lease != tail.row.lease || current.progress_dir != tail.row.progress_dir))
        || !matches!(current.state.as_str(), "running" | "ready")
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "Playback source changed",
        ));
    }
    tail.row = current;
    tail.listener.renew().await.map_err(io_error)?;
    tail.checked = std::time::Instant::now();
    Ok(())
}

async fn next(mut tail: Tail) -> Result<Option<(Bytes, Tail)>, std::io::Error> {
    loop {
        if tail.state.shutdown.is_cancelled()
            || tail.started.elapsed() > std::time::Duration::from_mins(32)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Playback stopped",
            ));
        }
        if tail.checked.elapsed() >= std::time::Duration::from_secs(1) {
            refresh(&mut tail).await?;
        }
        let mut bytes = vec![0_u8; 64 * 1024];
        let count = tokio::select! {()=tail.state.shutdown.cancelled()=>return Ok(None),count=tail.file.read(&mut bytes)=>count?};
        if tail.checked.elapsed() >= std::time::Duration::from_secs(1) {
            refresh(&mut tail).await?;
        }
        if count > 0 {
            bytes.truncate(count);
            return Ok(Some((Bytes::from(bytes), tail)));
        }
        if tail.row.state == "ready" {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
pub async fn serve(
    state: &AppState,
    asset: i64,
    access: Access,
    format: &str,
    bitrate: Option<u32>,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let selection = super::negotiate(state, access.viewer, format, bitrate).await?;
    let (row, listener) = super::request_cold(
        state,
        asset,
        access.viewer,
        selection,
        access.scope.as_ref(),
    )
    .await?;
    let format = Format::parse(&row.format)?;
    let first = std::time::Instant::now();
    let mut checked = first
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or(first);
    let mut current = row;
    loop {
        if first.elapsed() > std::time::Duration::from_secs(30) {
            return Err(ApiError::ServiceUnavailable(
                "Playback preparation is busy; try again shortly".into(),
            ));
        }
        if checked.elapsed() >= std::time::Duration::from_millis(500) {
            current = access.get(state, asset, current.id).await?;
            listener.renew().await?;
            checked = std::time::Instant::now();
        }
        if current.state == "ready" {
            return crate::routes::music_renditions::serve_ready(
                state,
                &current,
                headers,
                access.clone(),
            )
            .await;
        }
        if !matches!(current.state.as_str(), "queued" | "running") {
            return Err(ApiError::Unprocessable(
                "Could not prepare this playback format".into(),
            ));
        }
        if let Some(directory) = &current.progress_dir {
            // DB constraints and this independent check prevent path traversal
            // even if an operator repairs a row outside the normal worker.
            let valid = directory.strip_prefix("av-").is_some_and(|suffix| {
                suffix.len() >= 6
                    && suffix.len() <= 64
                    && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            });
            if !valid {
                return Err(ApiError::NotFound);
            }
            let path = state
                .config
                .media_dir
                .join("tmp")
                .join(directory)
                .join(format!("encoded.{}", format.extension()));
            if let Ok(file) = tokio::fs::File::open(path).await
                && file
                    .metadata()
                    .await
                    .map_err(|e| ApiError::Internal(Box::new(e)))?
                    .len()
                    > 0
            {
                let tail = Tail {
                    state: state.clone(),
                    access,
                    row: current,
                    listener,
                    file,
                    checked: std::time::Instant::now(),
                    started: first,
                };
                let mut response = (
                    [
                        (header::CONTENT_TYPE, format.content_type()),
                        (header::CACHE_CONTROL, "private, no-store"),
                        (header::VARY, "Authorization, Cookie"),
                        (header::ACCEPT_RANGES, "none"),
                        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    ],
                    Body::from_stream(futures_util::stream::try_unfold(tail, next)),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert("x-accel-buffering", "no".parse().unwrap());
                return Ok(response);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
