//! ZIP bodies retain current source, account and token authority after admission.
use crate::{AppState, error::ApiError};
use axum::{
    body::{Body, Bytes},
    http::HeaderMap,
    response::Response,
};
use futures_util::StreamExt as _;
struct Relay {
    state: AppState,
    owner: i64,
    token: i64,
    download: i64,
    body: axum::body::BodyDataStream,
    checked: std::time::Instant,
}
async fn token(state: &AppState, owner: i64, id: i64) -> Result<(), ApiError> {
    crate::music_bytes::Credential { owner, token: id }
        .check(state)
        .await?;
    Ok(())
}
async fn next(mut relay: Relay) -> Result<Option<(Bytes, Relay)>, std::io::Error> {
    if relay.state.shutdown.is_cancelled() {
        return Ok(None);
    }
    let bytes = relay
        .body
        .next()
        .await
        .transpose()
        .map_err(std::io::Error::other)?;
    if relay.state.shutdown.is_cancelled() {
        return Ok(None);
    }
    // Recheck after a potentially slow read, before releasing its bytes.
    if relay.checked.elapsed() >= std::time::Duration::from_secs(1) {
        token(&relay.state, relay.owner, relay.token)
            .await
            .map_err(std::io::Error::other)?;
        super::authorize_file(&relay.state, relay.owner, relay.download)
            .await
            .map_err(std::io::Error::other)?;
        relay.checked = std::time::Instant::now();
    }
    Ok(bytes.map(|bytes| (bytes, relay)))
}
pub async fn serve(
    state: &AppState,
    owner: i64,
    id: i64,
    credential: i64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    token(state, owner, credential).await?;
    let row = super::authorize_file(state, owner, id).await?;
    let key = row.file_name.as_ref().ok_or(ApiError::NotFound)?;
    let response = crate::routes::media::stream_stored_file(
        state,
        key,
        "application/zip",
        "private, no-store",
        headers,
    )
    .await?;
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"music-download-{id}.zip\"")
            .parse()
            .map_err(|_| ApiError::NotFound)?,
    );
    parts.headers.insert(
        axum::http::header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header"),
    );
    let relay = Relay {
        state: state.clone(),
        owner,
        token: credential,
        download: id,
        body: body.into_data_stream(),
        checked: std::time::Instant::now(),
    };
    Ok(Response::from_parts(
        parts,
        Body::from_stream(futures_util::stream::try_unfold(relay, next)),
    ))
}
