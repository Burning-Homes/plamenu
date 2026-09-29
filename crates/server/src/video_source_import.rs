//! Native, bounded source imports. No companion downloader and no automatic
//! publication. Network work never holds a transaction or buffers the movie.
use std::time::Duration;

use plamenu_db::{video_draft, video_source_import as db};
use tokio::task::JoinHandle;

use crate::{AppState, error::ApiError};

pub fn validate_url(raw: &str, allow_private: bool) -> Result<String, ApiError> {
    let url = url::Url::parse(raw.trim())
        .map_err(|_| ApiError::Unprocessable("Choose a valid public HTTPS URL".into()))?;
    if raw.len() > 2048
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::Unprocessable(
            "Use a URL without credentials or a fragment".into(),
        ));
    }
    plamenu_federation::guard::check_url_syntax(&url, allow_private)
        .map_err(|_| ApiError::Unprocessable("Choose a public HTTPS URL".into()))?;
    Ok(url.to_string())
}

/// Accept generic public `ActivityPub` Video objects, including `PeerTube`'s
/// public watch URLs. Do not fetch playlists/live sources, scripts, credentials
/// or a private object's file. Authentication cannot be supplied by an importer.
pub(crate) fn progressive_source(value: &serde_json::Value) -> Result<&str, ApiError> {
    let public = |field: &str| {
        value.get(field).is_some_and(|v| {
            v.as_str() == Some("https://www.w3.org/ns/activitystreams#Public")
                || v.as_array().is_some_and(|a| {
                    a.iter()
                        .any(|v| v.as_str() == Some("https://www.w3.org/ns/activitystreams#Public"))
                })
        })
    };
    if value["type"] != "Video"
        || value["isLiveBroadcast"] == true
        || !(public("to") || public("cc"))
    {
        return Err(ApiError::Unprocessable(
            "Choose a public, recorded ActivityPub Video".into(),
        ));
    }
    value["url"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|link| {
            std::iter::once(link).chain(
                link.get("tag")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(move |_| {
                        link["mediaType"] == "application/x-mpegURL"
                            || link["mediaType"] == "application/vnd.apple.mpegurl"
                    }),
            )
        })
        .filter(|v| v["mediaType"] == "video/mp4" || v["mediaType"] == "video/webm")
        .filter_map(|v| {
            v["href"]
                .as_str()
                .map(|url| (v["height"].as_u64().unwrap_or(0), url))
        })
        .max_by_key(|(height, _)| *height)
        .map(|(_, url)| url)
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "This Video does not offer a standard downloadable MP4 or WebM file".into(),
            )
        })
}

fn lost_claim() -> ApiError {
    ApiError::Conflict("Video import cancelled, expired or superseded".into())
}

/// Public for integration tests and explicit operator diagnostics. A failure
/// remains private and retryable; errors never include source URLs in logs/UI.
pub async fn run_one(state: &AppState) -> Result<bool, ApiError> {
    let Some(claim) = db::claim(&state.pool).await? else {
        return Ok(false);
    };
    let result = download(state, &claim).await;
    if result.is_err() && !state.shutdown.is_cancelled() {
        db::fail(&state.pool, &claim).await?;
    }
    result.map(|()| true)
}

async fn download(state: &AppState, claim: &db::Claim) -> Result<(), ApiError> {
    let scratch = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let file = tempfile::Builder::new()
        .prefix("video-upload-")
        .tempfile_in(scratch)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let fetched = fetch_source(state, claim, file.path()).await?;
    let actual = tokio::fs::metadata(file.path())
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
        .len();
    if actual == 0
        || actual != fetched.bytes_written
        || actual > u64::try_from(claim.reserved_bytes).unwrap_or(0)
    {
        return Err(ApiError::Unprocessable(
            "The source is empty or exceeds the reserved video limit".into(),
        ));
    }
    tokio::fs::File::open(file.path())
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
        .sync_all()
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    finish(state.clone(), claim.clone(), file, actual).await
}

async fn fetch_source(
    state: &AppState,
    claim: &db::Claim,
    path: &std::path::Path,
) -> Result<plamenu_federation::FetchedMediaFile, ApiError> {
    // Finish DB preflight before entering select!: an immediate lease tick
    // must not suspend a future holding the same publication lock it renews.
    let native = if claim.source_kind == "native" {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        let source = crate::video_source_edit::lock_source(&mut tx, claim).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Some(source)
    } else {
        None
    };
    let fetch = async {
        if let Some((key, expected)) = &native {
            return fetch_native(state, key, *expected, path).await;
        }
        let source = if claim.source_kind == "activitypub" {
            let object = state
                .federation
                .fetch_object(&claim.source_url)
                .await
                .map_err(|_| ApiError::Unprocessable("Could not fetch the public Video".into()))?;
            validate_url(
                progressive_source(&object)?,
                state.config.allow_private_fetch,
            )?
        } else {
            validate_url(&claim.source_url, state.config.allow_private_fetch)?
        };
        state
            .federation
            .fetch_video_source_to_file(
                &source,
                path,
                u64::try_from(claim.reserved_bytes).unwrap_or(0),
            )
            .await
            .map_err(|_| {
                ApiError::Unprocessable("Could not download the bounded video source".into())
            })
    };
    tokio::pin!(fetch);
    let mut renew = tokio::time::interval(Duration::from_secs(5));
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Stalled bodies have the transport's per-chunk limit; this also bounds an
    // origin that trickles forever. A failed import needs deliberate retry.
    let deadline = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            result = &mut fetch => return result,
            _ = renew.tick() => {
                if claim.source_kind=="native"{
                    let mut tx=state.pool.begin().await.map_err(plamenu_db::DbError::from)?;
                    crate::video_source_edit::lock_source(&mut tx,claim).await?;
                    tx.commit().await.map_err(plamenu_db::DbError::from)?;
                }
                if !db::renew(&state.pool, claim).await? { return Err(lost_claim()); }
            },
            () = &mut deadline => return Err(ApiError::Unprocessable("Video import timed out".into())),
            () = state.shutdown.cancelled() => return Err(lost_claim()),
        }
    }
}

async fn fetch_native(
    state: &AppState,
    key: &str,
    expected: i64,
    path: &std::path::Path,
) -> Result<plamenu_federation::FetchedMediaFile, ApiError> {
    use tokio::io::AsyncReadExt as _;
    let stored = state
        .media
        .open(key)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let limit = u64::try_from(expected).unwrap_or(0);
    if stored.len != limit || limit == 0 {
        return Err(ApiError::Conflict(
            "The retained source is incomplete; the published film was not changed".into(),
        ));
    }
    let mut reader = stored.reader.take(limit.saturating_add(1));
    let mut output = tokio::fs::File::create(path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let bytes_written = tokio::io::copy(&mut reader, &mut output)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok(plamenu_federation::FetchedMediaFile {
        final_url: "native".into(),
        content_type: "application/octet-stream".into(),
        bytes_written,
    })
}

async fn finish(
    state: AppState,
    claim: db::Claim,
    file: tempfile::NamedTempFile,
    actual: u64,
) -> Result<(), ApiError> {
    // Own the short rename/commit window if its caller is interrupted. The
    // receiving row fence serializes cancellation and acknowledged cleanup.
    let media_id = claim.media_id;
    let account_id = claim.account_id;
    tokio::spawn(async move {
        let mut tx = state
            .pool
            .begin()
            .await
            .map_err(plamenu_db::DbError::from)?;
        if claim.source_kind == "native" {
            crate::video_source_edit::lock_source(&mut tx, &claim).await?;
        }
        if !db::lock_claim(&mut tx, &claim).await? {
            return Err(lost_claim());
        }
        state
            .media
            .put_file(&format!("{media_id}.orig"), file.path())
            .await
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        if !video_draft::finish_receiving(
            &mut tx,
            media_id,
            account_id,
            i64::try_from(actual).unwrap_or(i64::MAX),
        )
        .await?
        {
            return Err(lost_claim());
        }
        db::complete(&mut tx, &claim).await?;
        tx.commit().await.map_err(plamenu_db::DbError::from)?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?
}

#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_one(&state).await {
                Ok(true) => {}
                Ok(false) => {
                    if !crate::workers::pause(&state, Duration::from_secs(2)).await {
                        return;
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "native video source import failed; private draft can be retried"
                    );
                    if !crate::workers::pause(&state, Duration::from_secs(2)).await {
                        return;
                    }
                }
            }
            if state.shutdown.is_cancelled() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_urls_and_public_recorded_objects_only() {
        for raw in [
            "file:///etc/passwd",
            "http://example.org/file.mp4",
            "https://127.0.0.1/file.mp4",
            "https://[::1]/file.mp4",
            "https://user:secret@example.org/file.mp4",
            "https://example.org/file.mp4#fragment",
        ] {
            assert!(validate_url(raw, false).is_err(), "{raw}");
        }
        assert!(validate_url("https://example.org/file.mp4?download=1", false).is_ok());
        let mut value = serde_json::json!({"type":"Video", "to":["https://www.w3.org/ns/activitystreams#Public"], "url":[
            {"mediaType":"application/x-mpegURL", "href":"https://example.org/master.m3u8"},
            {"mediaType":"video/mp4", "height":240, "href":"https://example.org/240.mp4"},
            {"mediaType":"video/mp4", "height":720, "href":"https://example.org/720.mp4"}]});
        assert_eq!(
            progressive_source(&value).unwrap(),
            "https://example.org/720.mp4"
        );
        value["isLiveBroadcast"] = true.into();
        assert!(progressive_source(&value).is_err());
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/peertube/create_video.json"))
                .unwrap();
        assert!(
            progressive_source(&fixture["object"])
                .unwrap()
                .ends_with("1080-fragmented.mp4")
        );
        value["isLiveBroadcast"] = false.into();
        value["to"] = serde_json::json!(["https://example.org/private-followers"]);
        assert!(progressive_source(&value).is_err());
    }
}
