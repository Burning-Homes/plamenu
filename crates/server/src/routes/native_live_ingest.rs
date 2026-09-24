//! Encrypted off-box FLV ingest through the existing HTTPS reverse proxy.
//! The publisher sends its RTMP-format key only in Authorization, never in a
//! URL or browser session. The HTTP body is streamed with bounded backpressure.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use futures_util::StreamExt;

use crate::error::ApiError;
use crate::live_ingest::{self, InputFormat};
use crate::state::AppState;

const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const CHUNK_BYTES: usize = 1024 * 1024;

/// The app's HTTP listener is intentionally plaintext behind a TLS proxy.
/// Only a configured trusted proxy may assert that the external hop was HTTPS;
/// a direct client spoofing X-Forwarded-Proto must not publish a stream key.
fn secure_proxy_hop(headers: &HeaderMap, peer: Option<IpAddr>, trusted: &[String]) -> bool {
    peer.is_some_and(|ip| {
        trusted
            .iter()
            .any(|cidr| crate::instance_policy::ip_matches_cidr(ip, cidr))
    }) && headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        == Some("https")
}

pub async fn publish(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    request: Request,
) -> Result<StatusCode, ApiError> {
    if !secure_proxy_hop(&headers, Some(peer.ip()), &state.config.trusted_proxies) {
        return Err(ApiError::Forbidden("HTTPS ingest is required".into()));
    }
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::Unauthorized("Stream key required".into()))?;
    let digest = live_ingest::key_digest(key)
        .ok_or_else(|| ApiError::Unauthorized("Invalid stream key".into()))?;
    let session = plamenu_db::native_live::claim(&state.pool, &digest)
        .await?
        .ok_or_else(|| ApiError::Unauthorized("Broadcast unavailable".into()))?;
    let sender = match live_ingest::start_piped(&state, session.clone(), InputFormat::HttpFlv).await
    {
        Ok(sender) => sender,
        Err(error) => {
            tracing::warn!(%error, media = session.media_id, "cannot prepare HTTPS live ingest");
            let _ = plamenu_db::native_live::finish(
                &state.pool,
                &session,
                plamenu_db::native_live::FinishReason::Interrupted,
            )
            .await;
            return Err(ApiError::ServiceUnavailable(
                "Media storage unavailable".into(),
            ));
        }
    };
    if let Err(error) = crate::native_live::federate_state(&state, session.status_id).await {
        tracing::warn!(%error, media = session.media_id, "could not announce HTTPS live start");
    }

    let mut body = request.into_body().into_data_stream();
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, body.next()).await {
            Ok(Some(Ok(bytes))) => {
                for chunk in bytes.chunks(CHUNK_BYTES) {
                    if sender.sender.send(chunk.to_vec()).await.is_err() {
                        return Err(ApiError::BadRequest("Unsupported FLV stream".into()));
                    }
                }
            }
            Ok(None) => {
                sender.mark_stopped();
                break;
            }
            Ok(Some(Err(error))) => {
                tracing::debug!(%error, "HTTPS live publisher disconnected");
                return Err(ApiError::BadRequest(
                    "Publisher connection interrupted".into(),
                ));
            }
            Err(_) => return Err(ApiError::BadRequest("Publisher timed out".into())),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_https_only_from_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        let trusted = vec!["127.0.0.0/8".to_owned()];
        assert!(secure_proxy_hop(
            &headers,
            Some("127.0.0.1".parse().unwrap()),
            &trusted
        ));
        assert!(!secure_proxy_hop(
            &headers,
            Some("203.0.113.7".parse().unwrap()),
            &trusted
        ));
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!secure_proxy_hop(
            &headers,
            Some("127.0.0.1".parse().unwrap()),
            &trusted
        ));
    }
}
