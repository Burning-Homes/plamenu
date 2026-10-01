//! Optional private processors. Only the origin owns claims and publication;
//! processor failure falls back to the unchanged native pipeline.
mod client;
pub(crate) mod protocol;

use crate::media_transcode::{PackagedHls, PackagedStoryboard, ProcessedAv, TranscodeParams};
use crate::{
    AppState,
    config::{ConfigError, SecretString},
    error::ApiError,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
pub struct Target {
    pub url: String,
    pub token: SecretString,
}
impl std::fmt::Debug for Target {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let origin = url::Url::parse(&self.url)
            .ok()
            .map(|url| url.origin().ascii_serialization());
        out.debug_struct("VideoProcessor")
            .field("origin", &origin)
            .field("token", &"[REDACTED]")
            .finish()
    }
}
impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FileTarget {
            url: String,
            token: String,
        }
        let file = FileTarget::deserialize(de)?;
        Ok(Self {
            url: file.url,
            token: file.token.into(),
        })
    }
}

pub(crate) fn validate_targets(targets: Vec<Target>) -> Result<Vec<Target>, ConfigError> {
    if targets.len() > 16 {
        return Err(invalid_config());
    }
    let mut unique = std::collections::HashSet::new();
    for target in &targets {
        let url = url::Url::parse(&target.url).map_err(|_| invalid_config())?;
        let loopback = url
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if target.url.len() > 2048
            || !unique.insert(url.as_str().to_owned())
            || (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !valid_token(target.token.expose())
        {
            return Err(invalid_config());
        }
    }
    Ok(targets)
}
pub(crate) fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len())
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn invalid_config() -> ConfigError {
    ConfigError::Invalid("video_offload", "use at most 16 distinct HTTPS origins (plaintext only on literal loopback), without URL credentials/query/path; supply a 32–256 character private token".into())
}

type Encoded = (ProcessedAv, Vec<PackagedHls>, PackagedStoryboard);
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(crate) async fn encode(
    state: &AppState,
    source: &str,
    params: &TranscodeParams,
    poster: f64,
    stem: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Encoded, ApiError> {
    let targets = &state.config.video_offload;
    if !targets.is_empty() {
        let start = NEXT.fetch_add(1, Ordering::Relaxed) % targets.len();
        for n in 0..targets.len() {
            let target = &targets[(start + n) % targets.len()];
            if let Ok(result) =
                client::encode(state, target, source, params, poster, stem, cancellation).await
            {
                tracing::info!(processor = ?target, renditions = result.1.len(), "native video offload output verified");
                return Ok(result);
            }
            if cancellation.is_cancelled() {
                return Err(ApiError::NotFound);
            }
            tracing::warn!(processor = ?target, "private video processor unavailable or invalid; trying next/native lane");
        }
    }
    let local = async {
        let processed =
            crate::media_worker::process_stored_native_video(state, source, params, poster).await?;
        if processed.content_type != "video/mp4" || processed.small.is_none() {
            return Err(ApiError::Unprocessable("Choose a video file".into()));
        }
        let hls =
            crate::media_transcode::package_native_hls(&state.config, &processed, stem, params)
                .await?;
        let board = crate::media_transcode::package_native_storyboard(
            &state.config,
            &processed,
            stem,
            params,
        )
        .await?;
        Ok((processed, hls, board))
    };
    tokio::select! { () = cancellation.cancelled() => Err(ApiError::NotFound), result = local => result }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_endpoint_validation_and_debug_do_not_disclose_tokens() {
        let token = "a".repeat(64);
        let target = |url: &str| Target {
            url: url.into(),
            token: token.clone().into(),
        };
        for url in ["http://127.0.0.1:8423/", "https://processor.example/"] {
            assert!(validate_targets(vec![target(url)]).is_ok());
        }
        for url in [
            "http://processor.example/",
            "http://0.0.0.0/",
            "https://user:secret@processor.example/",
            "https://processor.example/path",
            "https://processor.example/?token=secret",
        ] {
            assert!(validate_targets(vec![target(url)]).is_err());
        }
        assert!(!format!("{:?}", target("https://processor.example/")).contains(&token));
        assert!(
            validate_targets(vec![
                target("https://processor.example/"),
                target("https://processor.example/")
            ])
            .is_err()
        );
        assert!(!valid_token("short"));
    }
}
