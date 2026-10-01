//! Private transfer, explicit lease renewal and independently checked outputs.
//! Dropping this future stops transfer; the remote job expires within 30s.
use super::{
    Encoded, Target,
    protocol::{self, Artifact, Created, Manifest, Spec, Status},
};
use crate::{
    AppState,
    error::ApiError,
    media_transcode::{AvSmall, PackagedHls, PackagedStoryboard, ProcessedAv, TranscodeParams},
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Remote {
    http: reqwest::Client,
    target: Target,
    id: String,
}
impl Remote {
    fn request(&self, method: reqwest::Method, suffix: &str) -> reqwest::RequestBuilder {
        self.http
            .request(
                method,
                format!(
                    "{}v1/jobs/{}{}",
                    self.target.url.trim_end_matches('/').to_owned() + "/",
                    self.id,
                    suffix
                ),
            )
            .bearer_auth(self.target.token.expose())
    }
}

pub(super) async fn encode(
    state: &AppState,
    target: &Target,
    source: &str,
    params: &TranscodeParams,
    poster: f64,
    stem: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Encoded, ApiError> {
    let original = state.media.open(source).await.map_err(protocol::io_error)?;
    if original.len == 0 || original.len > params.max_av_bytes as u64 {
        return Err(protocol::invalid());
    }
    let spec = Spec::new(params, stem, original.len, poster)?;
    spec.validate(state.config.native_video)?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(10))
        .timeout(Duration::from_hours(4))
        .build()
        .map_err(http_error)?;
    let response = http
        .post(format!("{}/v1/jobs", target.url.trim_end_matches('/')))
        .bearer_auth(target.token.expose())
        .json(&spec)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(http_error)?;
    let created: Created =
        serde_json::from_slice(&limited(response, 1024).await?).map_err(|_| protocol::invalid())?;
    if created.id.len() != 33
        || !created.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || created.lease_seconds != protocol::LEASE_SECONDS
    {
        return Err(protocol::invalid());
    }
    let remote = Remote {
        http,
        target: target.clone(),
        id: created.id,
    };
    let transfer = async {
        let stream = tokio_util::io::ReaderStream::with_capacity(
            original.reader.take(original.len),
            64 * 1024,
        );
        let response = remote
            .request(reqwest::Method::PUT, "/source")
            .header(reqwest::header::CONTENT_LENGTH, original.len)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(http_error)?;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            return Err(protocol::invalid());
        }
        loop {
            let response = remote
                .request(reqwest::Method::GET, "")
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(http_error)?;
            let status: Status =
                serde_json::from_slice(&limited(response, protocol::MAX_MANIFEST).await?)
                    .map_err(|_| protocol::invalid())?;
            match status.state.as_str() {
                "ready" => {
                    return retrieve(
                        state,
                        &remote,
                        &spec,
                        status.manifest.ok_or_else(protocol::invalid)?,
                    )
                    .await;
                }
                "processing" => tokio::time::sleep(Duration::from_millis(500)).await,
                _ => return Err(protocol::invalid()),
            }
        }
    };
    let transfer = tokio::time::timeout(Duration::from_hours(4), transfer);
    tokio::pin!(transfer);
    let mut renew = tokio::time::interval(Duration::from_secs(5));
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            () = cancellation.cancelled() => break Err(ApiError::NotFound),
            result = &mut transfer => break result.unwrap_or_else(|_| Err(protocol::invalid())),
            _ = renew.tick() => {
                let renewed = remote.request(reqwest::Method::POST, "").timeout(Duration::from_secs(5)).send().await;
                if !renewed.is_ok_and(|response| response.status() == reqwest::StatusCode::NO_CONTENT) { break Err(protocol::invalid()); }
            }
        }
    };
    // No async Drop/unbounded cancellation queue: orderly completion releases
    // explicitly; cancellation or unreachable nodes use the bounded lease.
    let _ = remote
        .request(reqwest::Method::DELETE, "")
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    result
}
fn http_error(error: reqwest::Error) -> ApiError {
    ApiError::Internal(Box::new(error))
}
async fn limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, ApiError> {
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > limit as u64)
    {
        return Err(protocol::invalid());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(protocol::invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_manifest(spec: &Spec, manifest: &Manifest) -> Result<(), ApiError> {
    let count = manifest.renditions.len();
    if manifest.version != protocol::VERSION
        || count == 0
        || count > 6
        || manifest.artifacts.len() != count + 3
        || manifest
            .blurhash
            .as_ref()
            .is_some_and(|hash| hash.len() > 128)
    {
        return Err(protocol::invalid());
    }
    for (index, artifact) in manifest.artifacts.iter().enumerate() {
        let cap = match index {
            1 => crate::media_transcode::POSTER_FILE_LIMIT_BYTES,
            2 => crate::media_transcode::STORYBOARD_FILE_LIMIT_BYTES,
            _ => spec.output_bytes,
        };
        if artifact.bytes == 0
            || artifact.bytes > cap
            || artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(protocol::invalid());
        }
    }
    let mut heights = std::collections::HashSet::new();
    for rung in &manifest.renditions {
        if rung.width <= 0
            || rung.height <= 0
            || i64::from(rung.width) * i64::from(rung.height) > 8_294_400
            || rung.bandwidth <= 0
            || rung.bandwidth > 1_000_000_000
            || !heights.insert(rung.height)
            || rung.file_name != format!("{}.draft.hls{}.m4s", spec.stem, rung.height)
            || !safe_playlist(&rung.playlist, &rung.file_name)
        {
            return Err(protocol::invalid());
        }
    }
    let board = &manifest.board;
    if !(1..=10).contains(&board.columns)
        || !(1..=10).contains(&board.rows)
        || !(1..=100).contains(&board.count)
        || board.columns * board.rows < board.count
        || !(1..=604_800).contains(&board.interval_seconds)
    {
        return Err(protocol::invalid());
    }
    Ok(())
}
fn safe_playlist(playlist: &str, name: &str) -> bool {
    if playlist.len() > 4 * 1024 * 1024
        || !playlist.starts_with("#EXTM3U\n")
        || !playlist.contains("#EXT-X-ENDLIST")
    {
        return false;
    }
    let resource = format!("/media/{name}");
    for line in playlist.lines() {
        if !line.starts_with('#') {
            if line != resource {
                return false;
            }
            continue;
        }
        if ![
            "#EXTM3U",
            "#EXT-X-VERSION:",
            "#EXT-X-TARGETDURATION:",
            "#EXT-X-MEDIA-SEQUENCE:",
            "#EXT-X-PLAYLIST-TYPE:",
            "#EXT-X-MAP:",
            "#EXTINF:",
            "#EXT-X-BYTERANGE:",
            "#EXT-X-ENDLIST",
            "#EXT-X-INDEPENDENT-SEGMENTS",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            return false;
        }
        if line.contains("URI=")
            && (!line.starts_with("#EXT-X-MAP:")
                || !line.contains(&format!("URI=\"{resource}\""))
                || line.matches("URI=").count() != 1)
        {
            return false;
        }
    }
    true
}

async fn download(
    remote: &Remote,
    index: usize,
    artifact: &Artifact,
    path: &Path,
) -> Result<(), ApiError> {
    let mut response = remote
        .request(reqwest::Method::GET, &format!("/artifacts/{index}"))
        .send()
        .await
        .map_err(http_error)?;
    if response.status() != reqwest::StatusCode::OK
        || response.content_length() != Some(artifact.bytes)
    {
        return Err(protocol::invalid());
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(protocol::io_error)?;
    let mut bytes = 0_u64;
    let mut hash = Sha256::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .ok_or_else(protocol::invalid)?;
        if bytes > artifact.bytes || chunk.len() > 1024 * 1024 {
            return Err(protocol::invalid());
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(protocol::io_error)?;
    }
    let sha256 = protocol::hex(&hash.finalize());
    if bytes != artifact.bytes || sha256 != artifact.sha256 {
        return Err(protocol::invalid());
    }
    file.flush().await.map_err(protocol::io_error)
}
async fn retrieve(
    state: &AppState,
    remote: &Remote,
    spec: &Spec,
    manifest: Manifest,
) -> Result<Encoded, ApiError> {
    validate_manifest(spec, &manifest)?;
    let scratch = crate::media_transcode::scratch_dir(&state.config).await?;
    let mut paths = Vec::new();
    for (index, artifact) in manifest.artifacts.iter().enumerate() {
        let path = scratch.path().join(format!("artifact-{index}"));
        download(remote, index, artifact, &path).await?;
        paths.push(path);
    }
    let probe = crate::media_transcode::probe(&state.config, &paths[0]).await?;
    let (width, height) = checked_dimensions(&probe)?;
    let poster = tokio::fs::read(&paths[1])
        .await
        .map_err(protocol::io_error)?;
    let (poster_width, poster_height) =
        crate::media_processing::avif_dimensions(&poster).ok_or_else(protocol::invalid)?;
    if poster_width == 0
        || poster_height == 0
        || u64::from(poster_width) * u64::from(poster_height) > 8_294_400
    {
        return Err(protocol::invalid());
    }
    let board_dimensions = image::ImageReader::open(&paths[2])
        .map_err(protocol::io_error)?
        .with_guessed_format()
        .map_err(protocol::io_error)?
        .into_dimensions()
        .map_err(|_| protocol::invalid())?;
    if board_dimensions
        != (
            u32::try_from(manifest.board.columns * 160).map_err(|_| protocol::invalid())?,
            u32::try_from(manifest.board.rows * 90).map_err(|_| protocol::invalid())?,
        )
    {
        return Err(protocol::invalid());
    }
    let hls = validate_renditions(state, &manifest, &paths).await?;
    let board = PackagedStoryboard {
        file_path: paths[2].clone(),
        file_name: format!("{}.draft.storyboard.jpg", spec.stem),
        file_size: i64::try_from(manifest.artifacts[2].bytes).map_err(|_| protocol::invalid())?,
        columns: manifest.board.columns,
        rows: manifest.board.rows,
        count: manifest.board.count,
        interval_seconds: manifest.board.interval_seconds,
    };
    let processed = ProcessedAv {
        file_path: paths[0].clone(),
        file_size: i64::try_from(manifest.artifacts[0].bytes).map_err(|_| protocol::invalid())?,
        scratch,
        content_type: "video/mp4",
        extension: "mp4",
        kind: "video",
        width: Some(width),
        height: Some(height),
        duration: probe.duration,
        frame_rate: probe.frame_rate,
        bitrate: probe.bitrate,
        small: Some(AvSmall {
            bytes: poster,
            width: poster_width,
            height: poster_height,
        }),
        blurhash: manifest.blurhash,
    };
    Ok((processed, hls, board))
}
fn checked_dimensions(probe: &crate::media_transcode::Probe) -> Result<(i32, i32), ApiError> {
    let width = i32::try_from(probe.width.ok_or_else(protocol::invalid)?)
        .map_err(|_| protocol::invalid())?;
    let height = i32::try_from(probe.height.ok_or_else(protocol::invalid)?)
        .map_err(|_| protocol::invalid())?;
    if width <= 0
        || height <= 0
        || i64::from(width) * i64::from(height) > 8_294_400
        || probe.video_codec.as_deref() != Some("h264")
        || probe
            .audio_codec
            .as_deref()
            .is_some_and(|codec| codec != "aac")
        || probe
            .duration
            .is_none_or(|duration| !duration.is_finite() || duration <= 0.0)
        || probe
            .frame_rate_value()
            .is_none_or(|rate| !rate.is_finite() || rate <= 0.0 || rate > 120.0)
    {
        return Err(protocol::invalid());
    }
    Ok((width, height))
}
async fn validate_renditions(
    state: &AppState,
    manifest: &Manifest,
    paths: &[PathBuf],
) -> Result<Vec<PackagedHls>, ApiError> {
    let mut hls = Vec::new();
    for (index, rung) in manifest.renditions.iter().enumerate() {
        let path = paths[index + 3].clone();
        let probe = crate::media_transcode::probe(&state.config, &path).await?;
        if checked_dimensions(&probe)? != (rung.width, rung.height) {
            return Err(protocol::invalid());
        }
        let bytes =
            i64::try_from(manifest.artifacts[index + 3].bytes).map_err(|_| protocol::invalid())?;
        let hash_path = path.clone();
        let playlist = rung.playlist.clone();
        let name = rung.file_name.clone();
        let hashes = tokio::task::spawn_blocking(move || {
            crate::media_transcode::native_segment_hashes(&hash_path, &playlist, &name, bytes)
        })
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
        .map_err(|_| protocol::invalid())?;
        if hashes != rung.segment_sha256 {
            return Err(protocol::invalid());
        }
        hls.push(PackagedHls {
            file_path: path,
            file_name: rung.file_name.clone(),
            file_size: bytes,
            width: rung.width,
            height: rung.height,
            bandwidth: rung.bandwidth,
            playlist: rung.playlist.clone(),
            segment_sha256: hashes,
        });
    }
    Ok(hls)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_download_checks_actual_length_and_digest_before_use() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/v1/jobs/owned/artifacts/0",
            axum::routing::get(|| async { "verified private bytes" }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let remote = Remote {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            target: Target {
                url: format!("http://{address}/"),
                token: "a".repeat(64).into(),
            },
            id: "owned".into(),
        };
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("artifact");
        let expected = b"verified private bytes";
        let mut artifact = Artifact {
            bytes: expected.len() as u64,
            sha256: protocol::hex(&Sha256::digest(expected)),
        };
        download(&remote, 0, &artifact, &path).await.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), expected);
        artifact.sha256 = "0".repeat(64);
        assert!(download(&remote, 0, &artifact, &path).await.is_err());
        artifact.bytes += 1;
        assert!(download(&remote, 0, &artifact, &path).await.is_err());
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn manifest_rejects_unbounded_artifacts_forged_names_and_geometry() {
        let spec = Spec {
            version: protocol::VERSION,
            stem: "1".repeat(33),
            source_bytes: 10,
            output_bytes: 64 * crate::config::NativeVideoConfig::MIB,
            preset: "veryfast".into(),
            crf: Some(23),
            audio_kbps: 192,
            force_reencode: false,
            window_ms: None,
            poster_seconds: 0.0,
            avif_quality: 70,
            avif_speed: 6,
        };
        let name = format!("{}.draft.hls360.m4s", spec.stem);
        let base = Manifest {
            version: protocol::VERSION,
            artifacts: vec![
                Artifact {
                    bytes: 100,
                    sha256: "a".repeat(64)
                };
                4
            ],
            renditions: vec![protocol::Rendition {
                file_name: name.clone(),
                width: 640,
                height: 360,
                bandwidth: 1000,
                playlist: format!("#EXTM3U\n#EXTINF:1,\n/media/{name}\n#EXT-X-ENDLIST\n"),
                segment_sha256: serde_json::json!({}),
            }],
            board: protocol::Board {
                columns: 1,
                rows: 1,
                count: 1,
                interval_seconds: 1,
            },
            blurhash: None,
        };
        assert!(validate_manifest(&spec, &base).is_ok());
        let mut invalid = Vec::new();
        let mut item = base.clone();
        item.artifacts[0].bytes = spec.output_bytes + 1;
        invalid.push(item);
        let mut item = base.clone();
        item.artifacts[1].bytes = crate::media_transcode::POSTER_FILE_LIMIT_BYTES + 1;
        invalid.push(item);
        let mut item = base.clone();
        item.artifacts[0].sha256 = "bad".into();
        invalid.push(item);
        let mut item = base.clone();
        item.artifacts.push(item.artifacts[0].clone());
        invalid.push(item);
        let mut item = base.clone();
        item.renditions[0].file_name = "../private".into();
        invalid.push(item);
        let mut item = base.clone();
        item.renditions[0].width = i32::MAX;
        invalid.push(item);
        let mut item = base.clone();
        item.board.rows = i32::MAX;
        invalid.push(item);
        for manifest in invalid {
            assert!(validate_manifest(&spec, &manifest).is_err());
        }
    }

    #[test]
    fn playlist_metadata_cannot_introduce_remote_urls_keys_or_paths() {
        let valid = "#EXTM3U\n#EXT-X-MAP:URI=\"/media/a.m4s\",BYTERANGE=\"1@0\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:1@1\n/media/a.m4s\n#EXT-X-ENDLIST\n";
        assert!(safe_playlist(valid, "a.m4s"));
        for injected in [
            valid.replace("/media/a.m4s", "https://evil.example/a.m4s"),
            valid.replace(
                "#EXTINF:1,",
                "#EXT-X-KEY:METHOD=AES-128,URI=\"file:///secret\"",
            ),
            valid.replace(
                "#EXT-X-ENDLIST",
                "#EXT-X-MEDIA:URI=\"/private\"\n#EXT-X-ENDLIST",
            ),
        ] {
            assert!(!safe_playlist(&injected, "a.m4s"));
        }
    }
}
