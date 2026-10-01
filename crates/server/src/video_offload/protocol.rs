//! Versioned, bounded private wire shapes; never accept filesystem paths or
//! arbitrary source URLs from a job request.
use crate::{
    config::{Config, NativeVideoConfig},
    error::ApiError,
    media_transcode::TranscodeParams,
};
use serde::{Deserialize, Serialize};

pub const VERSION: u8 = 1;
pub const LEASE_SECONDS: u64 = 30;
pub const MAX_MANIFEST: usize = 32 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub version: u8,
    pub stem: String,
    pub source_bytes: u64,
    pub output_bytes: u64,
    pub preset: String,
    pub crf: Option<u8>,
    pub audio_kbps: u32,
    pub force_reencode: bool,
    pub window_ms: Option<[i64; 2]>,
    pub poster_seconds: f64,
    pub avif_quality: u8,
    pub avif_speed: u8,
}
impl Spec {
    pub fn new(
        params: &TranscodeParams,
        stem: &str,
        source_bytes: u64,
        poster_seconds: f64,
    ) -> Result<Self, ApiError> {
        Ok(Self {
            version: VERSION,
            stem: stem.into(),
            source_bytes,
            output_bytes: params
                .file_size_limit
                .as_ref()
                .ok_or(ApiError::NotFound)?
                .bytes,
            preset: params.preset.clone(),
            crf: params.crf,
            audio_kbps: params.audio_bitrate_kbps,
            force_reencode: params.force_reencode,
            window_ms: params
                .window
                .map(|window| [window.start_ms(), window.end_ms()]),
            poster_seconds,
            avif_quality: params.avif_quality,
            avif_speed: params.avif_speed_preview,
        })
    }
    pub fn params(&self, config: &Config) -> Result<TranscodeParams, ApiError> {
        self.validate(config.native_video)?;
        Ok(TranscodeParams {
            preset: self.preset.clone(),
            crf: self.crf,
            audio_bitrate_kbps: self.audio_kbps,
            force_reencode: self.force_reencode,
            window: self
                .window_ms
                .map(|[start, end]| crate::video_clip::Window::new(start, end))
                .transpose()?,
            max_av_bytes: usize::try_from(self.source_bytes)
                .map_err(|_| ApiError::PayloadTooLarge)?,
            file_size_limit: Some(crate::media_limits::FileSizeLimit {
                launcher: crate::media_limits::running_executable().map_err(io_error)?,
                bytes: self.output_bytes,
            }),
            avif_quality: self.avif_quality,
            avif_speed_preview: self.avif_speed,
            ..TranscodeParams::default()
        })
    }
    pub fn validate(&self, limits: NativeVideoConfig) -> Result<(), ApiError> {
        if self.version != VERSION
            || !(33..=64).contains(&self.stem.len())
            || !self.stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.source_bytes == 0
            || self.source_bytes > limits.max_upload_bytes()
            || self.output_bytes == 0
            || self.output_bytes > limits.output_limit(self.source_bytes)
            || ![
                "ultrafast",
                "superfast",
                "veryfast",
                "faster",
                "fast",
                "medium",
                "slow",
                "slower",
                "veryslow",
                "placebo",
            ]
            .contains(&self.preset.as_str())
            || self.crf.is_some_and(|crf| crf > 51)
            || !(32..=320).contains(&self.audio_kbps)
            || !self.poster_seconds.is_finite()
            || !(0.0..=604_800.0).contains(&self.poster_seconds)
            || !(1..=100).contains(&self.avif_quality)
            || !(1..=10).contains(&self.avif_speed)
        {
            return Err(invalid());
        }
        if let Some([start, end]) = self.window_ms {
            crate::video_clip::Window::new(start, end)?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rendition {
    pub file_name: String,
    pub width: i32,
    pub height: i32,
    pub bandwidth: i64,
    pub playlist: String,
    pub segment_sha256: serde_json::Value,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Board {
    pub columns: i32,
    pub rows: i32,
    pub count: i32,
    pub interval_seconds: i32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u8,
    /// Fixed indices: delivery MP4, AVIF, storyboard JPEG, then HLS rungs.
    pub artifacts: Vec<Artifact>,
    pub renditions: Vec<Rendition>,
    pub board: Board,
    pub blurhash: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub state: String,
    pub manifest: Option<Manifest>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Created {
    pub id: String,
    pub lease_seconds: u64,
}
pub fn invalid() -> ApiError {
    ApiError::Unprocessable("Invalid private processor job or output".into())
}
pub fn io_error(error: std::io::Error) -> ApiError {
    ApiError::Internal(Box::new(error))
}

pub async fn digest(path: &std::path::Path) -> Result<Artifact, ApiError> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let bytes = file.metadata().await.map_err(io_error)?.len();
    let mut digest = Sha256::new();
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut chunk).await.map_err(io_error)?;
        if n == 0 {
            break;
        }
        digest.update(&chunk[..n]);
    }
    let sha256 = hex(&digest.finalize());
    Ok(Artifact { bytes, sha256 })
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").expect("string formatting");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Spec {
        Spec {
            version: VERSION,
            stem: "1".repeat(33),
            source_bytes: 10,
            output_bytes: 64 * NativeVideoConfig::MIB,
            preset: "veryfast".into(),
            crf: Some(23),
            audio_kbps: 192,
            force_reencode: false,
            window_ms: None,
            poster_seconds: 0.0,
            avif_quality: 70,
            avif_speed: 6,
        }
    }

    #[test]
    fn private_specs_refuse_unbounded_jobs_paths_and_invalid_media_controls() {
        let limits = NativeVideoConfig::default();
        let base = spec();
        assert!(base.validate(limits).is_ok());
        let mut invalid = Vec::new();
        for bytes in [0, limits.max_upload_bytes() + 1] {
            let mut item = base.clone();
            item.source_bytes = bytes;
            invalid.push(item);
        }
        for bytes in [0, limits.output_limit(10) + 1] {
            let mut item = base.clone();
            item.output_bytes = bytes;
            invalid.push(item);
        }
        for stem in ["../private", "/tmp/source", "short", &"g".repeat(33)] {
            let mut item = base.clone();
            item.stem = stem.into();
            invalid.push(item);
        }
        for poster in [f64::NAN, f64::INFINITY, -1.0, 604_801.0] {
            let mut item = base.clone();
            item.poster_seconds = poster;
            invalid.push(item);
        }
        for window in [[-1, 1000], [1000, 1000], [1000, 0]] {
            let mut item = base.clone();
            item.window_ms = Some(window);
            invalid.push(item);
        }
        let mut version = base.clone();
        version.version = 2;
        invalid.push(version);
        let mut preset = base.clone();
        preset.preset = "-i file:///private".into();
        invalid.push(preset);
        let mut crf = base.clone();
        crf.crf = Some(52);
        invalid.push(crf);
        for item in invalid {
            assert!(item.validate(limits).is_err());
        }
        let mut json = serde_json::to_value(base).unwrap();
        json["source_url"] = "file:///private".into();
        assert!(serde_json::from_value::<Spec>(json).is_err());
    }
}
