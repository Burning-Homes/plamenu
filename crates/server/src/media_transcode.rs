//! Video/audio upload processing via ffmpeg/ffprobe subprocesses —
//! Mastodon's `Paperclip::Transcoder` pipeline: videos become H.264/AAC
//! mp4s (stream-copied when already compatible), animated GIFs become
//! soundless mp4s (`gifv`), audio becomes mp3; a poster frame provides the
//! `small` style and the blurhash. `-map_metadata -1` strips file metadata
//! on every path, like the image re-encode does.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::config::Config;
use crate::error::ApiError;
use crate::media_processing::blurhash_for;

/// ffprobe answers in milliseconds on sane inputs; a longer run means a
/// pathological file or a broken binary. Killed, not waited out — one hung
/// subprocess must never stall a worker lane (chaotic-federation rule).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Poster extraction demuxes a single frame; generous for cold disks.
const FRAME_TIMEOUT: Duration = Duration::from_mins(2);
/// Wall-clock ceiling for a full encode or a remux of a multi-gigabyte
/// file. Anything slower than this is effectively hung for our purposes:
/// the upload/caching flow it serves has long since been abandoned.
const ENCODE_TIMEOUT: Duration = Duration::from_mins(30);

/// Mastodon's `MAX_VIDEO_MATRIX_LIMIT` (3840x2160).
const MAX_VIDEO_MATRIX: i64 = 8_294_400;
/// Mastodon's `MAX_VIDEO_FRAME_RATE`.
const MAX_FRAME_RATE: f64 = 120.0;
/// Mastodon's `MAX_VIDEO_FRAMES` (~5 minutes at 120 fps).
const MAX_VIDEO_FRAMES: &str = "36000";
/// H.264 "High" bits per pixel, Mastodon's bitrate heuristic.
const BITS_PER_PIXEL: f64 = 0.11;

/// ffprobe `format_name` values we accept — Mastodon's video + audio MIME
/// list, expressed as containers (`wav` covers the `audio/x-wav` family,
/// the mp4 muxer family covers m4a/3gp, `asf` is wma).
const ACCEPTED_FORMATS: &[&str] = &[
    "mov,mp4,m4a,3gp,3g2,mj2",
    "matroska,webm",
    "ogg",
    "gif",
    "mp3",
    "flac",
    "wav",
    "aac",
    "asf",
];

/// What an upload turned out to be once probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvKind {
    Video,
    /// Animated GIFs and soundless videos, Mastodon's `gifv`.
    Gifv,
    Audio,
}

/// The operator's A/V transcode knobs (M39), read from the instance settings
/// once per job. [`Default`] mirrors the historical constants.
#[derive(Clone, Debug)]
pub struct TranscodeParams {
    /// x264 preset for re-encodes.
    pub preset: String,
    /// `Some(crf)` = constant-quality mode; `None` = Mastodon's ABR budget.
    pub crf: Option<u8>,
    /// AAC bitrate (kbps) for transcoded sound tracks.
    pub audio_bitrate_kbps: u32,
    /// Longest soundless video still classified `gifv`.
    pub gifv_max_seconds: f64,
    /// Upload byte cap; also the ABR budget's size ceiling.
    pub max_av_bytes: usize,
    /// Poster-frame AVIF encode settings.
    pub avif_quality: u8,
    pub avif_speed_preview: u8,
}

impl Default for TranscodeParams {
    fn default() -> Self {
        Self {
            preset: "veryfast".to_owned(),
            crf: None,
            audio_bitrate_kbps: 192,
            gifv_max_seconds: 60.0,
            max_av_bytes: crate::media_processing::MAX_AV_UPLOAD_BYTES,
            avif_quality: 70,
            avif_speed_preview: 6,
        }
    }
}

impl TranscodeParams {
    /// Bundles the operator's settings for one transcode run, clamped
    /// defensively (the admin form already validates).
    #[must_use]
    pub fn from_settings(settings: &plamenu_db::instance_settings::InstanceSettings) -> Self {
        Self {
            preset: settings.media_video_preset.clone(),
            crf: (settings.media_video_rate_mode == "crf")
                .then(|| u8::try_from(settings.media_video_crf.clamp(0, 51)).unwrap_or(23)),
            audio_bitrate_kbps: u32::try_from(settings.media_audio_bitrate_kbps.clamp(32, 320))
                .unwrap_or(192),
            gifv_max_seconds: f64::from(settings.media_gifv_max_seconds.clamp(1, 3600)),
            max_av_bytes: usize::try_from(settings.media_max_av_mb.clamp(1, 99))
                .unwrap_or(99)
                .saturating_mul(1024 * 1024),
            avif_quality: u8::try_from(settings.media_avif_quality.clamp(1, 100)).unwrap_or(70),
            avif_speed_preview: u8::try_from(settings.media_avif_speed_preview.clamp(1, 10))
                .unwrap_or(6),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    format_name: String,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    #[serde(default)]
    disposition: Value,
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    format: Option<ProbeFormat>,
    #[serde(default)]
    streams: Vec<ProbeStream>,
    error: Option<Value>,
}

/// The parsed essentials of an ffprobe run (Mastodon's
/// `VideoMetadataExtractor`).
#[derive(Debug, Default)]
pub struct Probe {
    pub format_name: String,
    pub duration: Option<f64>,
    pub bitrate: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub colorspace: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// The raw frame-rate fraction (`"30/1"`), `avg_frame_rate` falling
    /// back to `r_frame_rate` like Mastodon.
    pub frame_rate: Option<String>,
    pub r_frame_rate: Option<f64>,
}

impl Probe {
    pub fn frame_rate_value(&self) -> Option<f64> {
        self.frame_rate.as_deref().and_then(parse_fraction)
    }
}

fn parse_fraction(raw: &str) -> Option<f64> {
    let (num, den) = raw.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    (den != 0.0).then_some(num / den)
}

/// A non-zero frame-rate fraction, or `None` for `"0/0"` streams.
fn usable_fraction(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    parse_fraction(raw).filter(|rate| *rate > 0.0)?;
    Some(raw.to_owned())
}

fn unsupported() -> ApiError {
    ApiError::Unprocessable("Validation failed: File content type is not supported".into())
}

fn transcode_failed(detail: String) -> ApiError {
    ApiError::Internal(detail.into())
}

/// Runs ffprobe on the file and parses the metadata Mastodon reads.
pub async fn probe(config: &Config, path: &Path) -> Result<Probe, ApiError> {
    // `kill_on_drop` reaps the subprocess when the timeout (or a cancelled
    // caller) drops this future — a wedged ffprobe must not linger.
    let run = Command::new(&config.ffprobe_path)
        .args(["-i"])
        .arg(path)
        .args([
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_error",
            "-loglevel",
            "fatal",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(PROBE_TIMEOUT, run)
        .await
        .map_err(|_| transcode_failed(format!("ffprobe timed out after {PROBE_TIMEOUT:?}")))?
        .map_err(|e| transcode_failed(format!("could not run ffprobe: {e}")))?;
    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout).map_err(|error| {
        // Unparseable ffprobe JSON is an operational failure (broken build,
        // OOM kill), not a bad upload — surfaced in the log even though the
        // client still sees the same 422 as for a hostile file.
        tracing::error!(%error, "ffprobe produced unparseable output");
        unsupported()
    })?;
    if parsed.error.is_some() || !output.status.success() {
        return Err(unsupported());
    }

    let mut result = Probe::default();
    if let Some(format) = parsed.format {
        result.format_name = format.format_name;
        result.duration = format.duration.as_deref().and_then(|d| d.parse().ok());
        result.bitrate = format.bit_rate.as_deref().and_then(|b| b.parse().ok());
    }
    // Cover art rides files as a video stream flagged `attached_pic`; it
    // must not make an audio file a video.
    let video = parsed.streams.iter().find(|s| {
        s.codec_type == "video"
            && s.disposition.get("attached_pic").and_then(Value::as_i64) != Some(1)
    });
    if let Some(stream) = video {
        result.video_codec.clone_from(&stream.codec_name);
        result.colorspace.clone_from(&stream.pix_fmt);
        result.width = stream.width;
        result.height = stream.height;
        result.frame_rate = usable_fraction(stream.avg_frame_rate.as_deref())
            .or_else(|| usable_fraction(stream.r_frame_rate.as_deref()));
        result.r_frame_rate = stream.r_frame_rate.as_deref().and_then(parse_fraction);
    }
    if let Some(stream) = parsed.streams.iter().find(|s| s.codec_type == "audio") {
        result.audio_codec.clone_from(&stream.codec_name);
    }
    Ok(result)
}

/// Classifies a probed non-image upload and applies Mastodon's upload-time
/// validations (accepted container, matrix and frame-rate limits). A
/// soundless video only counts as `gifv` (auto-playing, looping) when it is
/// short like one — `gifv_max_seconds`, same rule as the remote path.
pub fn classify(probe: &Probe, gifv_max_seconds: f64) -> Result<AvKind, ApiError> {
    if !ACCEPTED_FORMATS.contains(&probe.format_name.as_str()) {
        return Err(unsupported());
    }
    let has_video = probe.video_codec.is_some();
    if has_video {
        let (Some(width), Some(height)) = (probe.width, probe.height) else {
            return Err(ApiError::Unprocessable("Video has no video stream".into()));
        };
        if probe.frame_rate.is_none() {
            return Err(ApiError::Unprocessable("Video has no video stream".into()));
        }
        if width <= 0 || height <= 0 {
            return Err(ApiError::Unprocessable("Invalid video dimensions".into()));
        }
        let matrix = width
            .checked_mul(height)
            .ok_or_else(|| ApiError::Unprocessable("Invalid video dimensions".into()))?;
        if matrix > MAX_VIDEO_MATRIX {
            return Err(ApiError::Unprocessable(format!(
                "{width}x{height} videos are not supported"
            )));
        }
        if let Some(rate) = probe.frame_rate_value()
            && rate.floor() > MAX_FRAME_RATE
        {
            #[allow(clippy::cast_possible_truncation)]
            let fps = rate.floor() as i64;
            return Err(ApiError::Unprocessable(format!(
                "{fps}fps videos are not supported"
            )));
        }
        // Animated GIFs and short soundless videos are gifv, like Mastodon's
        // `gif_transcoder` / `update_attachment_type`; a soundless hour-long
        // video is still a video (unknown duration counts as long).
        if probe.format_name == "gif"
            || (probe.audio_codec.is_none()
                && probe.duration.unwrap_or(f64::MAX) <= gifv_max_seconds)
        {
            return Ok(AvKind::Gifv);
        }
        return Ok(AvKind::Video);
    }
    if probe.audio_codec.is_some() {
        return Ok(AvKind::Audio);
    }
    Err(unsupported())
}

/// Runs one ffmpeg operation with a hard wall-clock ceiling. `kill_on_drop`
/// covers both the timeout and a cancelled caller (a media-proxy request
/// that hit its own deadline, a disconnected upload): the subprocess dies
/// with the future instead of encoding on detached — which also means the
/// dropped `InflightGuard` can no longer race a live orphan.
async fn run_ffmpeg(
    config: &Config,
    args: Vec<std::ffi::OsString>,
    timeout: Duration,
) -> Result<(), ApiError> {
    let run = Command::new(&config.ffmpeg_path)
        .args(["-nostdin", "-loglevel", "fatal"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| transcode_failed(format!("ffmpeg timed out after {timeout:?}")))?
        .map_err(|e| transcode_failed(format!("could not run ffmpeg: {e}")))?;
    if !output.status.success() {
        return Err(transcode_failed(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn os_args(args: &[&str]) -> Vec<std::ffi::OsString> {
    args.iter().map(Into::into).collect()
}

/// Mastodon's `VIDEO_PASSTHROUGH_OPTIONS`: already-compatible files are
/// stream-copied instead of re-encoded.
fn eligible_for_passthrough(probe: &Probe) -> bool {
    probe.video_codec.as_deref() == Some("h264")
        && matches!(probe.audio_codec.as_deref(), None | Some("aac"))
        && matches!(probe.colorspace.as_deref(), Some("yuv420p" | "yuvj420p"))
}

/// Transcodes (or stream-copies) a video to the delivery mp4.
async fn transcode_video(
    config: &Config,
    probe: &Probe,
    input: &Path,
    output: &Path,
    params: &TranscodeParams,
) -> Result<(), ApiError> {
    let passthrough = eligible_for_passthrough(probe);
    // Stream copies are I/O-bound and stay ungated; a real re-encode waits
    // for a heavy-work slot so concurrent uploads can't each grab every core.
    let _permit = if passthrough {
        None
    } else {
        Some(crate::media_gate::acquire().await)
    };
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), input.into()];
    if passthrough {
        args.extend(os_args(&[
            "-map_metadata",
            "-1",
            "-movflags",
            "faststart",
            "-c:v",
            "copy",
            "-c:a",
            "copy",
        ]));
    } else {
        let audio_bitrate = i64::from(params.audio_bitrate_kbps) * 1000;
        args.extend(os_args(&[
            "-preset",
            params.preset.as_str(),
            "-movflags",
            "faststart",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "crop=floor(iw/2)*2:floor(ih/2)*2",
            "-c:v",
            "h264",
            "-c:a",
            "aac",
            "-map_metadata",
            "-1",
            "-frames:v",
            MAX_VIDEO_FRAMES,
        ]));
        args.push("-b:a".into());
        args.push(format!("{}k", params.audio_bitrate_kbps).into());
        if let Some(crf) = params.crf {
            // Constant quality: bytes go where the picture needs them.
            args.push("-crf".into());
            args.push(crf.to_string().into());
        } else {
            // Mastodon's bitrate budget: H.264-High bits-per-pixel at 30 fps,
            // capped so the file fits the upload limit.
            let width = probe.width.unwrap_or(0).max(2);
            let height = probe.height.unwrap_or(0).max(2);
            let duration = probe.duration.unwrap_or(1.0).max(1.0);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let desired = ((width * height * 30) as f64 * BITS_PER_PIXEL).floor() as i64;
            let size_limit_bits = i64::try_from(params.max_av_bytes)
                .unwrap_or(i64::MAX)
                .saturating_mul(8);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let maximum = (size_limit_bits as f64 / duration).floor() as i64 - audio_bitrate;
            // libx264 counts kbps and refuses a rate that rounds to zero, so
            // tiny frames get a floor Mastodon never needs.
            let bitrate = desired.min(maximum).max(1_000);
            args.push("-b:v".into());
            args.push(bitrate.to_string().into());
            args.push("-maxrate".into());
            args.push((bitrate + audio_bitrate).to_string().into());
            args.push("-bufsize".into());
            args.push((bitrate * 5).to_string().into());
        }
        if probe.r_frame_rate.is_some_and(|rate| rate > MAX_FRAME_RATE) {
            args.extend(os_args(&["-fps_mode", "vfr"]));
        }
    }
    args.push("-y".into());
    args.push(output.into());
    run_ffmpeg(config, args, ENCODE_TIMEOUT).await
}

/// Extracts the poster frame (frame 0, scaled into 640x640) — the `small`
/// style of Mastodon's `VIDEO_STYLES`.
async fn extract_frame(config: &Config, input: &Path, output: &Path) -> Result<(), ApiError> {
    let mut args: Vec<std::ffi::OsString> =
        vec!["-ss".into(), "0".into(), "-i".into(), input.into()];
    args.extend(os_args(&[
        "-vf",
        "scale='min(640,iw)':'min(640,ih)':force_original_aspect_ratio=decrease",
        "-f",
        "image2",
        "-frames:v",
        "1",
    ]));
    args.push("-y".into());
    args.push(output.into());
    run_ffmpeg(config, args, FRAME_TIMEOUT).await
}

/// Transcodes audio to the delivery mp3 (Mastodon's `AUDIO_STYLES`;
/// `-vn` drops embedded cover art).
async fn transcode_audio(config: &Config, input: &Path, output: &Path) -> Result<(), ApiError> {
    let _permit = crate::media_gate::acquire().await;
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), input.into()];
    args.extend(os_args(&["-vn", "-map_metadata", "-1", "-q:a", "2"]));
    args.push("-y".into());
    args.push(output.into());
    run_ffmpeg(config, args, ENCODE_TIMEOUT).await
}

/// The poster-frame rendition of a processed video.
pub struct AvSmall {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A fully processed video/audio upload, ready to store. The output stays on
/// disk in its own scratch directory (`_scratch` keeps it alive until the
/// caller has moved `file_path` into the store) — buffering a 99 MiB video in
/// RAM per job was a tmpfs-grade memory spike.
pub struct ProcessedAv {
    pub file_path: std::path::PathBuf,
    pub file_size: i64,
    /// Owns the scratch directory holding `file_path`.
    pub scratch: tempfile::TempDir,
    pub content_type: &'static str,
    pub extension: &'static str,
    pub kind: &'static str,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub duration: Option<f64>,
    pub frame_rate: Option<String>,
    pub bitrate: Option<i64>,
    pub small: Option<AvSmall>,
    pub blurhash: Option<String>,
}

/// A processed remote A/V download. Unlike [`ProcessedAv`] the output stays
/// on disk (`file_path`, inside the caller's scratch dir) — remote videos can
/// be gigabytes, and the caller moves the file into the store instead of
/// buffering it.
pub struct RemoteAv {
    pub file_path: std::path::PathBuf,
    pub file_size: i64,
    pub content_type: &'static str,
    pub extension: &'static str,
    pub kind: &'static str,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub duration: Option<f64>,
    pub frame_rate: Option<String>,
    pub bitrate: Option<i64>,
    pub small: Option<AvSmall>,
    pub blurhash: Option<String>,
}

/// Whether a remote video can be stream-copied into the delivery mp4. Remote
/// caching never re-encodes video — a multi-hour re-encode would starve the
/// host — so anything else is refused and the row keeps serving its origin
/// URL (the viewer's direct-remote preference still reaches it).
fn remux_eligible(probe: &Probe) -> bool {
    ACCEPTED_FORMATS.contains(&probe.format_name.as_str())
        && probe.video_codec.as_deref() == Some("h264")
        && matches!(probe.colorspace.as_deref(), Some("yuv420p" | "yuvj420p"))
}

/// Audio codecs an mp4 container serves to browsers as-is; anything else
/// (opus, vorbis, …) transcodes to AAC — an audio-only encode is cheap even
/// for hours of material.
fn audio_copy_ok(codec: Option<&str>) -> bool {
    matches!(codec, Some("aac" | "mp3"))
}

/// Processes a downloaded remote A/V pair into the delivery file, on disk:
/// video stream-copied (never re-encoded — see [`remux_eligible`]) and muxed
/// with `audio_input` when the origin ships separated audio (`PeerTube` 6.x
/// HLS), plus the usual poster frame, blurhash and probed `meta`. An
/// audio-only object falls back to the mp3 pipeline. Everything happens in
/// `work_dir` (the caller's scratch directory, which must outlive the result).
#[allow(
    clippy::too_many_lines,
    reason = "one linear probe → remux → poster flow"
)]
pub async fn process_remote_av(
    config: &Config,
    work_dir: &Path,
    video_input: &Path,
    audio_input: Option<&Path>,
    params: &TranscodeParams,
) -> Result<RemoteAv, ApiError> {
    let probed = probe(config, video_input).await?;

    if probed.video_codec.is_none() {
        if probed.audio_codec.is_none() {
            return Err(unsupported());
        }
        let output = work_dir.join("remote-out.mp3");
        transcode_audio(config, video_input, &output).await?;
        let out_probe = probe(config, &output).await?;
        let file_size = file_len(&output).await?;
        return Ok(RemoteAv {
            file_path: output,
            file_size,
            content_type: "audio/mpeg",
            extension: "mp3",
            kind: "audio",
            width: None,
            height: None,
            duration: out_probe.duration,
            frame_rate: None,
            bitrate: out_probe.bitrate,
            small: None,
            blurhash: None,
        });
    }

    if !remux_eligible(&probed) {
        return Err(unsupported());
    }
    // Pick the audio: the separated companion stream when there is one,
    // otherwise whatever the video file itself carries.
    let (audio_path, audio_codec) = match audio_input {
        Some(path) => {
            let audio_probe = probe(config, path).await?;
            if audio_probe.audio_codec.is_none() {
                return Err(unsupported());
            }
            (Some(path), audio_probe.audio_codec)
        }
        None => (
            probed.audio_codec.is_some().then_some(video_input),
            probed.audio_codec.clone(),
        ),
    };

    let output = work_dir.join("remote-out.mp4");
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), video_input.into()];
    let separate_audio = audio_path.is_some_and(|path| path != video_input);
    if separate_audio {
        args.push("-i".into());
        args.push(audio_path.unwrap_or(video_input).into());
    }
    args.extend(os_args(&["-map", "0:v:0"]));
    if audio_path.is_some() {
        args.extend(os_args(&[
            "-map",
            if separate_audio { "1:a:0" } else { "0:a:0" },
        ]));
    }
    args.extend(os_args(&["-c:v", "copy"]));
    if audio_path.is_some() {
        if audio_copy_ok(audio_codec.as_deref()) {
            args.extend(os_args(&["-c:a", "copy"]));
        } else {
            args.extend(os_args(&["-c:a", "aac"]));
            args.push("-b:a".into());
            args.push(format!("{}k", params.audio_bitrate_kbps).into());
        }
    }
    args.extend(os_args(&["-map_metadata", "-1", "-movflags", "faststart"]));
    args.push("-y".into());
    args.push(output.as_os_str().into());
    run_ffmpeg(config, args, ENCODE_TIMEOUT).await?;

    let out_probe = probe(config, &output).await?;
    let frame_path = work_dir.join("remote-frame.png");
    extract_frame(config, &output, &frame_path).await?;
    let frame_bytes = tokio::fs::read(&frame_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let frame =
        image::load_from_memory(&frame_bytes).map_err(|e| ApiError::Internal(Box::new(e)))?;
    let blurhash = blurhash_for(&frame);
    let small = AvSmall {
        width: frame.width(),
        height: frame.height(),
        bytes: crate::media_processing::encode_preview_avif(
            &frame,
            params.avif_speed_preview,
            params.avif_quality,
        )?,
    };
    let soundless = audio_path.is_none();
    let short = out_probe.duration.unwrap_or(f64::MAX) <= params.gifv_max_seconds;
    let file_size = file_len(&output).await?;
    Ok(RemoteAv {
        file_path: output,
        file_size,
        content_type: "video/mp4",
        extension: "mp4",
        kind: if soundless && short { "gifv" } else { "video" },
        width: out_probe.width.and_then(|w| i32::try_from(w).ok()),
        height: out_probe.height.and_then(|h| i32::try_from(h).ok()),
        duration: out_probe.duration,
        frame_rate: out_probe.frame_rate,
        bitrate: out_probe.bitrate,
        small: Some(small),
        blurhash,
    })
}

async fn file_len(path: &Path) -> Result<i64, ApiError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    Ok(i64::try_from(meta.len()).unwrap_or(i64::MAX))
}

/// Writes the upload to scratch space, probes and classifies it. Used by
/// the upload endpoint to validate before processing (or queueing).
pub async fn classify_upload(
    config: &Config,
    bytes: &[u8],
    params: &TranscodeParams,
) -> Result<AvKind, ApiError> {
    if bytes.len() > params.max_av_bytes {
        return Err(ApiError::Unprocessable(
            "Validation failed: File size exceeds the limit".into(),
        ));
    }
    let dir = scratch_dir(config).await?;
    let input = dir.path().join("in.bin");
    tokio::fs::write(&input, bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let probed = probe(config, &input).await?;
    classify(&probed, params.gifv_max_seconds)
}

/// Re-encodes an animated GIF as an animated WebP (`libwebp_anim`) — the
/// alpha-safe, universally displayable modern target for cached remote GIFs.
/// Returns the WebP bytes; the caller falls back to keeping the GIF when the
/// operator's ffmpeg lacks the encoder.
pub async fn gif_to_animated_webp(config: &Config, gif: &[u8]) -> Result<Vec<u8>, ApiError> {
    let _permit = crate::media_gate::acquire().await;
    let dir = scratch_dir(config).await?;
    let input = dir.path().join("in.gif");
    let output = dir.path().join("out.webp");
    tokio::fs::write(&input, gif)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), input.into()];
    args.extend(os_args(&[
        "-c:v",
        "libwebp_anim",
        "-lossless",
        "0",
        "-q:v",
        "80",
        "-loop",
        "0",
        "-an",
        "-f",
        "webp",
    ]));
    args.push("-y".into());
    args.push(output.as_os_str().into());
    run_ffmpeg(config, args, ENCODE_TIMEOUT).await?;
    tokio::fs::read(&output)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

/// Scratch space under the media root, like the on-demand A/V lane: the
/// output moves into the store by rename, and a large intermediate never
/// lands in a RAM-backed `/tmp`.
async fn scratch_dir(config: &Config) -> Result<tempfile::TempDir, ApiError> {
    let root = config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    tempfile::Builder::new()
        .prefix("av-")
        .tempdir_in(&root)
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

/// Runs the full pipeline on an uploaded video/audio file: validate,
/// transcode, probe the output for `meta`, and (for video) extract the
/// poster frame for the `small` style and blurhash.
pub async fn process_av(
    config: &Config,
    bytes: &[u8],
    params: &TranscodeParams,
) -> Result<ProcessedAv, ApiError> {
    if bytes.len() > params.max_av_bytes {
        return Err(ApiError::Unprocessable(
            "Validation failed: File size exceeds the limit".into(),
        ));
    }
    let dir = scratch_dir(config).await?;
    let input = dir.path().join("in.bin");
    tokio::fs::write(&input, bytes)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let probed = probe(config, &input).await?;
    let declared = classify(&probed, params.gifv_max_seconds)?;

    match declared {
        AvKind::Video | AvKind::Gifv => {
            let output = dir.path().join("out.mp4");
            transcode_video(config, &probed, &input, &output, params).await?;
            // `meta.original` reflects the delivered file, like Mastodon's
            // `populate_meta` probing `queued_for_write`.
            let out_probe = probe(config, &output).await?;

            let frame_path = dir.path().join("frame.png");
            extract_frame(config, &output, &frame_path).await?;
            let frame_bytes = tokio::fs::read(&frame_path)
                .await
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            let frame = image::load_from_memory(&frame_bytes)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            let blurhash = blurhash_for(&frame);
            // Re-encode the ffmpeg PNG frame as AVIF — the poster is a
            // feed-visible preview, so it gets the same size win as an image's
            // small style.
            let small = AvSmall {
                width: frame.width(),
                height: frame.height(),
                bytes: crate::media_processing::encode_preview_avif(
                    &frame,
                    params.avif_speed_preview,
                    params.avif_quality,
                )?,
            };

            let file_size = file_len(&output).await?;
            Ok(ProcessedAv {
                file_path: output,
                file_size,
                scratch: dir,
                content_type: "video/mp4",
                extension: "mp4",
                kind: if declared == AvKind::Gifv {
                    "gifv"
                } else {
                    "video"
                },
                width: out_probe.width.and_then(|w| i32::try_from(w).ok()),
                height: out_probe.height.and_then(|h| i32::try_from(h).ok()),
                duration: out_probe.duration,
                frame_rate: out_probe.frame_rate,
                bitrate: out_probe.bitrate,
                small: Some(small),
                blurhash,
            })
        }
        AvKind::Audio => {
            let output = dir.path().join("out.mp3");
            transcode_audio(config, &input, &output).await?;
            let out_probe = probe(config, &output).await?;
            let file_size = file_len(&output).await?;
            Ok(ProcessedAv {
                file_path: output,
                file_size,
                scratch: dir,
                content_type: "audio/mpeg",
                extension: "mp3",
                kind: "audio",
                width: None,
                height: None,
                duration: out_probe.duration,
                frame_rate: None,
                bitrate: out_probe.bitrate,
                small: None,
                blurhash: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(width: i64, height: i64) -> Probe {
        Probe {
            format_name: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
            video_codec: Some("h264".to_owned()),
            width: Some(width),
            height: Some(height),
            frame_rate: Some("30/1".to_owned()),
            ..Probe::default()
        }
    }

    fn is_invalid_dimensions(result: Result<AvKind, ApiError>) -> bool {
        matches!(
            result,
            Err(ApiError::Unprocessable(message)) if message == "Invalid video dimensions"
        )
    }

    #[test]
    fn video_matrix_validation_is_positive_checked_and_bounded() {
        for (width, height) in [
            (0, 1),
            (1, 0),
            (-1, 1),
            (1, -1),
            (i64::MAX, 2),
            (2, i64::MAX),
        ] {
            assert!(
                is_invalid_dimensions(classify(&video(width, height), 30.0)),
                "{width}x{height} must fail closed"
            );
        }

        let width = 4_096;
        let at_limit = MAX_VIDEO_MATRIX / width;
        assert_eq!(width * at_limit, MAX_VIDEO_MATRIX);
        assert_eq!(
            classify(&video(width, at_limit), 30.0).unwrap(),
            AvKind::Video
        );
        assert!(matches!(
            classify(&video(width, at_limit + 1), 30.0),
            Err(ApiError::Unprocessable(message)) if message.contains("not supported")
        ));
    }
}
