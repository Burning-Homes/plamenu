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
use crate::media_processing::{avif_dimensions, blurhash_for, push_avif_encoder_args, read_pam};

/// ffprobe answers in milliseconds on sane inputs; a longer run means a
/// pathological file or a broken binary. Killed, not waited out — one hung
/// subprocess must never stall a worker lane (chaotic-federation rule).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROBE_BYTES: u64 = 256 * 1024;
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
/// Native scratch reservations include two independently bounded poster files
/// (the AVIF and a tiny hash frame), not an assumed multiplier of input size.
pub const POSTER_FILE_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
/// Copy-only source quality plus at most five useful lower, never upscaled rungs.
pub const NATIVE_HLS_MAX_RENDITIONS: u64 = 6;
pub const STORYBOARD_FILE_LIMIT_BYTES: u64 = 2 * 1024 * 1024;
const NATIVE_HLS_HEIGHTS: [i32; 5] = [1080, 720, 480, 360, 240];
/// H.264 "High" bits per pixel, Mastodon's bitrate heuristic.
const BITS_PER_PIXEL: f64 = 0.11;

/// ffprobe `format_name` values we accept — Mastodon's video + audio MIME
/// list, expressed as containers (`wav` covers the `audio/x-wav` family,
/// the mp4 muxer family covers m4a/3gp, `asf` is wma).
pub(crate) const ACCEPTED_FORMATS: &[&str] = &[
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
    /// Native job output ceiling, enforced in the encoder process by the OS.
    /// Ordinary attachment callers leave this unset until their quotas migrate.
    pub file_size_limit: Option<crate::media_limits::FileSizeLimit>,
    /// Explicit creator reprocessing must use current encoder settings, even
    /// when the retained source could otherwise be cheaply stream-copied.
    pub force_reencode: bool,
    /// Accurate native edit window. Encoding discards input-seek pre-roll;
    /// stream copy is forbidden so an earlier keyframe cannot escape a trim.
    pub window: Option<crate::video_clip::Window>,
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
            file_size_limit: None,
            force_reencode: false,
            window: None,
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
            file_size_limit: None,
            force_reencode: false,
            window: None,
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
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
    #[serde(default)]
    codec_type: String,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    channels: Option<i32>,
    channel_layout: Option<String>,
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
    pub tags: std::collections::HashMap<String, String>,
    pub format_name: String,
    pub duration: Option<f64>,
    pub bitrate: Option<i64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub has_artwork: bool,
    pub audio_sample_rate: Option<i32>,
    pub audio_channels: Option<i32>,
    pub audio_layout: Option<String>,
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
    probe_with_tags(config, path, false).await
}

pub(crate) async fn probe_music(config: &Config, path: &Path) -> Result<Probe, ApiError> {
    probe_with_tags(config, path, true).await
}

async fn probe_with_tags(config: &Config, path: &Path, tags: bool) -> Result<Probe, ApiError> {
    use tokio::io::AsyncReadExt;
    // `kill_on_drop` reaps the subprocess when the timeout (or a cancelled
    // caller) drops this future — a wedged ffprobe must not linger.
    let mut child = Command::new(&config.ffprobe_path)
        .args(local_input_options_for(path))
        .args(["-i"])
        .arg(path)
        .args([
            "-print_format",
            "json",
            "-show_entries",
            if tags {"format=format_name,duration,bit_rate:format_tags:stream=codec_type,codec_name,sample_rate,channels,channel_layout,pix_fmt,width,height,avg_frame_rate,r_frame_rate:stream_tags:stream_disposition=attached_pic"} else {"format=format_name,duration,bit_rate:stream=codec_type,codec_name,pix_fmt,width,height,avg_frame_rate,r_frame_rate:stream_disposition=attached_pic"},
            "-show_error",
            "-loglevel",
            "fatal",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| transcode_failed(format!("could not run ffprobe: {e}")))?;
    let run = async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout was piped")
            .take(MAX_PROBE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| transcode_failed(format!("could not read ffprobe: {e}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROBE_BYTES {
            return Err(unsupported());
        }
        let status = child
            .wait()
            .await
            .map_err(|e| transcode_failed(format!("could not wait for ffprobe: {e}")))?;
        Ok((status, bytes))
    };
    let (status, bytes) = tokio::time::timeout(PROBE_TIMEOUT, run)
        .await
        .map_err(|_| transcode_failed(format!("ffprobe timed out after {PROBE_TIMEOUT:?}")))??;
    let parsed: ProbeOutput = serde_json::from_slice(&bytes).map_err(|error| {
        // Unparseable ffprobe JSON is an operational failure (broken build,
        // OOM kill), not a bad upload — surfaced in the log even though the
        // client still sees the same 422 as for a hostile file.
        tracing::error!(%error, "ffprobe produced unparseable output");
        unsupported()
    })?;
    if parsed.error.is_some() || !status.success() {
        return Err(unsupported());
    }

    let mut result = Probe::default();
    if let Some(format) = parsed.format {
        result.format_name = format.format_name;
        result.duration = format.duration.as_deref().and_then(|d| d.parse().ok());
        result.bitrate = format.bit_rate.as_deref().and_then(|b| b.parse().ok());
        result.tags = format.tags;
    }
    // Cover art rides files as a video stream flagged `attached_pic`; it
    // must not make an audio file a video.
    result.has_artwork = parsed.streams.iter().any(|stream| {
        stream.codec_type == "video"
            && stream
                .disposition
                .get("attached_pic")
                .and_then(Value::as_i64)
                == Some(1)
    });
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
        result.audio_sample_rate = stream.sample_rate.as_deref().and_then(|v| v.parse().ok());
        result.audio_channels = stream.channels;
        result.audio_layout.clone_from(&stream.channel_layout);
        if tags {
            // Vorbis/Opus keep music tags on their audio stream rather than
            // the container. Container fields take precedence when both exist.
            result.tags = result
                .tags
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect();
            for (key, value) in &stream.tags {
                result
                    .tags
                    .entry(key.to_ascii_lowercase())
                    .or_insert_with(|| value.clone());
            }
        }
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
pub(crate) async fn run_ffmpeg(
    config: &Config,
    args: Vec<std::ffi::OsString>,
    timeout: Duration,
    limit: Option<&crate::media_limits::FileSizeLimit>,
) -> Result<(), ApiError> {
    let mut args = restrict_local_inputs(args);
    if limit.is_some() {
        // Native worker slots already bound concurrency. Prevent each codec
        // from auto-allocating frame threads for every host CPU as well.
        // Input decoder and output encoder options have separate scopes.
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let threads = (cores / config.native_video.jobs.saturating_mul(2).max(1))
            .clamp(1, 8)
            .to_string();
        let mut scoped = os_args(&["-filter_threads", "1", "-filter_complex_threads", "1"]);
        for arg in args {
            if arg == "-i" || arg == "-c:v" {
                scoped.extend(os_args(&["-threads", &threads]));
            }
            scoped.push(arg);
        }
        args = scoped;
    }
    let mut child = crate::media_limits::command(Path::new(&config.ffmpeg_path), limit)
        .args(["-nostdin", "-loglevel", "fatal"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| transcode_failed(format!("could not run ffmpeg: {e}")))?;
    let mut stderr = child.stderr.take().expect("stderr was piped");
    // Keep a bounded diagnostic prefix while draining the pipe to avoid both
    // an unbounded error buffer and an encoder blocked on a full stderr pipe.
    let diagnostics = async move {
        use tokio::io::AsyncReadExt;
        let mut prefix = Vec::new();
        (&mut stderr)
            .take(16 * 1024)
            .read_to_end(&mut prefix)
            .await?;
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        Ok::<_, std::io::Error>(prefix)
    };
    let run = async { tokio::try_join!(child.wait(), diagnostics) };
    let (status, stderr) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| transcode_failed(format!("ffmpeg timed out after {timeout:?}")))?
        .map_err(|e| transcode_failed(format!("could not run ffmpeg: {e}")))?;
    if !status.success() {
        return Err(transcode_failed(format!(
            "ffmpeg failed ({status}): {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(())
}

fn os_args(args: &[&str]) -> Vec<std::ffi::OsString> {
    args.iter().map(Into::into).collect()
}

/// Reject manifest/script demuxers before they can open referenced resources,
/// not merely after probing has already performed their I/O. These paths only
/// process files already fetched through the application's network safeguards.
/// Live/HLS networking is handled separately, never by these upload commands.
pub(crate) fn local_input_options() -> Vec<std::ffi::OsString> {
    os_args(&[
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        &ACCEPTED_FORMATS.join(","),
    ])
}

fn local_input_options_for(path: &Path) -> Vec<std::ffi::OsString> {
    let mut options = local_input_options();
    if trusted_prefix_input(path) {
        options[1] = "file,subfile".into();
    }
    options
}

/// The only extra protocol input is our own held regular descriptor with a
/// decimal finite end. No arbitrary pathname, foreign PID or URL is accepted.
fn trusted_prefix_input(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some(raw) = path
            .to_str()
            .and_then(|raw| raw.strip_prefix("subfile,,start,0,end,"))
        else {
            return false;
        };
        let Some((end, descriptor)) = raw.split_once(",,:") else {
            return false;
        };
        if end.is_empty() || !end.bytes().all(|value| value.is_ascii_digit()) {
            return false;
        }
        let Ok(end) = end.parse::<u64>() else {
            return false;
        };
        let base = format!("/proc/{}/fd/", std::process::id());
        let Some(fd) = descriptor.strip_prefix(&base) else {
            return false;
        };
        if fd.is_empty() || !fd.bytes().all(|value| value.is_ascii_digit()) {
            return false;
        }
        std::fs::metadata(descriptor)
            .is_ok_and(|metadata| metadata.is_file() && end > 0 && end <= metadata.len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// Input options reset after each `-i`; separated remote audio must receive the
/// same boundary as video. Arguments and paths are generated internally.
fn restrict_local_inputs(args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut bounded = Vec::with_capacity(args.len() + 4);
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if arg == "-i" {
            bounded.extend(args.peek().map_or_else(local_input_options, |path| {
                local_input_options_for(Path::new(path))
            }));
        }
        bounded.push(arg);
    }
    bounded
}

/// Mastodon's `VIDEO_PASSTHROUGH_OPTIONS`: already-compatible files are
/// stream-copied instead of re-encoded.
fn eligible_for_passthrough(probe: &Probe) -> bool {
    probe.video_codec.as_deref() == Some("h264")
        && matches!(probe.audio_codec.as_deref(), None | Some("aac"))
        && matches!(probe.colorspace.as_deref(), Some("yuv420p" | "yuvj420p"))
}

fn edit_input_args(input: &Path, params: &TranscodeParams) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    if let Some(window) = params.window {
        // Input seek finds a nearby keyframe efficiently. FFmpeg's accurate
        // seek discards pre-roll during re-encoding; never combine with copy.
        args.extend(os_args(&[
            "-ss",
            &window.start_argument(),
            "-accurate_seek",
        ]));
    }
    args.extend(["-i".into(), input.into()]);
    if let Some(window) = params.window {
        args.extend(os_args(&["-t", &window.duration_argument()]));
    }
    args
}

/// Transcodes (or stream-copies) a video to the delivery mp4.
async fn transcode_video(
    config: &Config,
    probe: &Probe,
    input: &Path,
    output: &Path,
    params: &TranscodeParams,
) -> Result<(), ApiError> {
    let passthrough =
        eligible_for_passthrough(probe) && !params.force_reencode && params.window.is_none();
    // Stream copies are I/O-bound and stay ungated; a real re-encode waits
    // for a heavy-work slot so concurrent uploads can't each grab every core.
    let _permit = if passthrough {
        None
    } else {
        Some(crate::media_gate::acquire().await)
    };
    let mut args = edit_input_args(input, params);
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
        ]));
        args.push("-b:a".into());
        args.push(format!("{}k", params.audio_bitrate_kbps).into());
        if params.force_reencode || params.window.is_some() {
            // These native edits already pay for an encode. Keep the copy-top
            // HLS rendition seekable at the same four-second cadence as the
            // lower ladder, rather than inheriting x264's 25s default GOP.
            args.extend(os_args(&[
                "-force_key_frames",
                "expr:gte(t,n_forced*4)",
                "-sc_threshold",
                "0",
            ]));
        }
        if let Some(crf) = params.crf {
            // Constant quality: bytes go where the picture needs them.
            args.push("-crf".into());
            args.push(crf.to_string().into());
        } else {
            // Mastodon's bitrate budget: H.264-High bits-per-pixel at 30 fps,
            // capped so the file fits the upload limit.
            let width = probe.width.unwrap_or(0).max(2);
            let height = probe.height.unwrap_or(0).max(2);
            let duration = params
                .window
                .map_or_else(
                    || probe.duration.unwrap_or(1.0),
                    crate::video_clip::Window::duration_seconds,
                )
                .max(1.0);
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
    run_ffmpeg(
        config,
        args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await
}

/// Produces a video's AVIF poster and its tiny blurhash source in one `FFmpeg`
/// decode. The old path decoded into PNG, decoded that again in Rust, then
/// launched `FFmpeg` a second time to encode AVIF.
async fn extract_poster(
    config: &Config,
    work_dir: &Path,
    input: &Path,
    params: &TranscodeParams,
) -> Result<(AvSmall, Option<String>), ApiError> {
    extract_poster_at(config, work_dir, input, params, 0.0).await
}

async fn extract_poster_at(
    config: &Config,
    work_dir: &Path,
    input: &Path,
    params: &TranscodeParams,
    seek_seconds: f64,
) -> Result<(AvSmall, Option<String>), ApiError> {
    let _permit = crate::media_gate::acquire().await;
    let poster_path = work_dir.join("poster.avif");
    let hash_path = work_dir.join("poster-hash.pam");
    let args = poster_ffmpeg_args(input, &poster_path, &hash_path, params, seek_seconds);
    let poster_limit = params.file_size_limit.clone().map(|mut limit| {
        limit.bytes = limit.bytes.min(POSTER_FILE_LIMIT_BYTES);
        limit
    });
    run_ffmpeg(config, args, FRAME_TIMEOUT, poster_limit.as_ref()).await?;

    let bytes = tokio::fs::read(&poster_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let (width, height) = avif_dimensions(&bytes)
        .ok_or_else(|| transcode_failed("ffmpeg returned an invalid AVIF poster".into()))?;
    let hash_image = read_pam(&hash_path)?;
    Ok((
        AvSmall {
            bytes,
            width,
            height,
        },
        blurhash_for(&hash_image),
    ))
}

/// An HTML-only author can enter a time without knowing the file's exact
/// duration. Keep the request within the decoded stream rather than failing
/// the durable job after the entire source has been uploaded and transcoded.
pub(crate) fn poster_seek_seconds(requested: f64, duration: Option<f64>) -> f64 {
    let Some(duration) = duration.filter(|value| value.is_finite() && *value > 0.0) else {
        return 0.0;
    };
    if !requested.is_finite() || requested <= 0.0 {
        return 0.0;
    }
    // Seek at least one second before EOF. Sparse/low-frame-rate encodes may
    // have no decodable frame in the final fraction of a second.
    requested.min((duration - 1.0).max(0.0))
}

fn poster_ffmpeg_args(
    input: &Path,
    poster_path: &Path,
    hash_path: &Path,
    params: &TranscodeParams,
    seek_seconds: f64,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-ss".into(),
        format!("{seek_seconds:.3}").into(),
        "-i".into(),
        input.into(),
    ];
    args.extend([
        "-filter_complex".into(),
        "[0:v:0]scale='min(640,iw)':'min(640,ih)':force_original_aspect_ratio=decrease,split=2[poster][hash_source];[hash_source]scale='min(64,iw)':'min(64,ih)':force_original_aspect_ratio=decrease[hash]".into(),
        "-map".into(),
        "[poster]".into(),
    ]);
    push_avif_encoder_args(&mut args, params.avif_speed_preview, params.avif_quality);
    args.extend(os_args(&[
        "-pix_fmt",
        "yuv444p",
        "-color_range",
        "pc",
        "-colorspace",
        "smpte170m",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "iec61966-2-1",
        "-map_metadata",
        "-1",
        "-frames:v",
        "1",
        "-f",
        "avif",
        "-y",
    ]));
    args.push(poster_path.as_os_str().into());
    args.extend(os_args(&[
        "-map",
        "[hash]",
        "-frames:v",
        "1",
        "-c:v",
        "pam",
        "-pix_fmt",
        "rgba",
        "-f",
        "image2",
        "-y",
    ]));
    args.push(hash_path.as_os_str().into());
    args
}

/// Transcodes audio to the delivery mp3 (Mastodon's `AUDIO_STYLES`;
/// `-vn` drops embedded cover art).
async fn transcode_audio(
    config: &Config,
    input: &Path,
    output: &Path,
    params: &TranscodeParams,
) -> Result<(), ApiError> {
    let _permit = crate::media_gate::acquire().await;
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), input.into()];
    args.extend(os_args(&["-vn", "-map_metadata", "-1", "-q:a", "2"]));
    args.push("-y".into());
    args.push(output.into());
    run_ffmpeg(
        config,
        args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await
}

/// PeerTube-compatible Video authoring from an audio-only source. A low-frame-
/// rate waveform keeps the result visibly identifiable as audio, without a
/// companion image generator or a full-rate static-picture encode. Every four
/// seconds has a keyframe so HLS seeking is not hostage to a long audio file.
async fn transcode_audio_visualization(
    config: &Config,
    input: &Path,
    output: &Path,
    params: &TranscodeParams,
) -> Result<(), ApiError> {
    let _permit = crate::media_gate::acquire().await;
    let mut args = edit_input_args(input, params);
    args.extend(os_args(&[
        "-filter_complex",
        "[0:a:0]showwaves=s=640x360:mode=line:rate=2:colors=0x80aaff:scale=sqrt,format=yuv420p[v]",
        "-map",
        "[v]",
        "-map",
        "0:a:0",
        "-c:v",
        "libx264",
        "-preset",
        params.preset.as_str(),
        "-crf",
        "28",
        "-r",
        "2",
        "-g",
        "8",
        "-keyint_min",
        "8",
        "-sc_threshold",
        "0",
        "-c:a",
        "aac",
        "-b:a",
        &format!("{}k", params.audio_bitrate_kbps),
        "-map_metadata",
        "-1",
        "-movflags",
        "+faststart",
        "-shortest",
        "-y",
    ]));
    args.push(output.into());
    run_ffmpeg(
        config,
        args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await
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

/// One immutable, single-file fMP4 HLS rendition built beside the delivery
/// MP4 in the same scratch directory. Its playlist is small transactional
/// metadata; `file_path` stays on disk until the worker's fenced store step.
pub struct PackagedHls {
    pub file_path: std::path::PathBuf,
    pub file_name: String,
    pub file_size: i64,
    pub width: i32,
    pub height: i32,
    pub bandwidth: i64,
    pub playlist: String,
    pub segment_sha256: serde_json::Value,
    pub progressive: Option<PackagedProgressive>,
}

pub struct PackagedProgressive {
    pub file_path: std::path::PathBuf,
    pub metadata: plamenu_db::native_video_hls::Progressive,
}

/// The ladder's height means encoded height, including portrait films.
/// A copied original only qualifies through its measured bitrate. Unknown
/// sizes never establish the lighter-than-original fallback condition.
#[must_use]
pub fn progressive_choice(
    candidates: &[(i32, Option<u32>, Option<i64>)],
    original_bytes: i64,
) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, (h, rate, _))| *h > 0 && *h <= 720 && rate.is_some_and(|r| r > 0 && r <= 4000))
        .max_by_key(|(_, (h, _, _))| *h)
        .map(|(i, _)| i)
        .or_else(|| {
            candidates
                .iter()
                .enumerate()
                .filter(|(_, (h, _, size))| {
                    *h > 0 && size.is_some_and(|s| s > 0 && s < original_bytes)
                })
                .min_by_key(|(_, (h, _, _))| *h)
                .map(|(i, _)| i)
        })
}

fn native_video_rate(height: i32) -> u32 {
    match height {
        1080 => 6000,
        720 => 3000,
        480 => 1800,
        360 => 1000,
        _ => 500,
    }
}

/// One bounded JPEG sprite, generated from keyframes without decoding every
/// frame of a long film. Tiles are approximate navigation aids, not alt text.
pub struct PackagedStoryboard {
    pub file_path: std::path::PathBuf,
    pub file_name: String,
    pub file_size: i64,
    pub columns: i32,
    pub rows: i32,
    pub count: i32,
    pub interval_seconds: i32,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "ceil/clamp produce positive integer-valued preview grid counts and bounded intervals"
)]
pub async fn package_native_storyboard(
    config: &Config,
    processed: &ProcessedAv,
    stem: &str,
    params: &TranscodeParams,
) -> Result<PackagedStoryboard, ApiError> {
    let duration = processed
        .duration
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| transcode_failed("Video has no finite duration for seek previews".into()))?;
    let interval_seconds = (duration / 100.0).ceil().clamp(1.0, 604_800.0) as i32;
    let count = (duration / f64::from(interval_seconds))
        .ceil()
        .clamp(1.0, 100.0) as i32;
    let columns = count.min(10);
    let rows = (count + columns - 1) / columns;
    let cells = columns * rows;
    let file_path = processed.scratch.path().join("storyboard.jpg");
    let filter = format!(
        "fps=1/{interval_seconds}:start_time=0:round=up:eof_action=pass,scale=160:90:force_original_aspect_ratio=decrease,pad=160:90:(ow-iw)/2:(oh-ih)/2,tpad=stop_mode=clone:stop=100,tile={columns}x{rows}:nb_frames={cells}"
    );
    let mut args = os_args(&[
        "-nostdin",
        "-v",
        "error",
        "-threads",
        "1",
        "-skip_frame",
        "nokey",
        "-i",
    ]);
    args.push(processed.file_path.as_os_str().into());
    args.extend(os_args(&[
        "-an",
        "-map_metadata",
        "-1",
        "-vf",
        &filter,
        "-frames:v",
        "1",
        "-q:v",
        "3",
        "-y",
    ]));
    args.push(file_path.as_os_str().into());
    let limit = params
        .file_size_limit
        .as_ref()
        .map(|limit| crate::media_limits::FileSizeLimit {
            launcher: limit.launcher.clone(),
            bytes: limit.bytes.min(STORYBOARD_FILE_LIMIT_BYTES),
        });
    let _permit = crate::media_gate::acquire().await;
    run_ffmpeg(config, args, ENCODE_TIMEOUT, limit.as_ref()).await?;
    let size = tokio::fs::metadata(&file_path)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
        .len();
    if size == 0 || size > STORYBOARD_FILE_LIMIT_BYTES {
        return Err(transcode_failed(
            "Seek previews exceeded their bounded JPEG limit".into(),
        ));
    }
    Ok(PackagedStoryboard {
        file_path,
        file_name: format!("{stem}.draft.storyboard.jpg"),
        file_size: i64::try_from(size).unwrap_or(i64::MAX),
        columns,
        rows,
        count,
        interval_seconds,
    })
}

/// Package a copy-only top quality and a bounded, sequential lower ladder.
/// The former avoids a redundant full-resolution transcode; the latter gives
/// restricted connections a genuinely smaller rendition. The ordinary MP4
/// remains independently playable in clients without HLS or JavaScript.
pub async fn package_native_hls(
    config: &Config,
    processed: &ProcessedAv,
    stem: &str,
    params: &TranscodeParams,
) -> Result<Vec<PackagedHls>, ApiError> {
    let height = processed.height.unwrap_or(0);
    if processed.content_type != "video/mp4" || height <= 0 {
        return Err(ApiError::Unprocessable(
            "Video has no usable dimensions".into(),
        ));
    }
    let lower: Vec<_> = native_lower_heights(height)
        .filter(|lower| {
            i64::from(processed.width.unwrap_or(0)) * i64::from(*lower) / i64::from(height) >= 2
        })
        .collect();
    let mut candidates = vec![(
        height,
        processed
            .bitrate
            .and_then(|b| u32::try_from(b.saturating_add(999) / 1000).ok()),
        Some(processed.file_size),
    )];
    candidates.extend(
        lower
            .iter()
            .map(|h| (*h, Some(native_video_rate(*h)), None)),
    );
    let selected = progressive_choice(&candidates, processed.file_size).map(|i| candidates[i].0);
    let mut renditions = Vec::with_capacity(NATIVE_HLS_HEIGHTS.len() + 1);
    renditions.push(package_one_hls(config, processed, stem, params, height, None, false).await?);
    for lower in lower {
        renditions.push(
            package_one_hls(
                config,
                processed,
                stem,
                params,
                lower,
                Some(lower),
                selected == Some(lower),
            )
            .await?,
        );
    }
    Ok(renditions)
}

fn native_lower_heights(height: i32) -> impl Iterator<Item = i32> {
    // Do not add a nearly identical rung (e.g. 240p beneath a 250p source).
    NATIVE_HLS_HEIGHTS
        .into_iter()
        .filter(move |lower| i64::from(*lower) * 10 <= i64::from(height) * 9)
}

fn native_hls_args(
    processed: &ProcessedAv,
    playlist_path: &Path,
    params: &TranscodeParams,
    target_height: Option<i32>,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), processed.file_path.clone().into()];
    args.extend(os_args(&[
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        "-map_metadata",
        "-1",
    ]));
    if let Some(height) = target_height {
        let scale = format!("scale=-2:{height}:flags=bicubic");
        let rate = native_video_rate(height);
        let maxrate = format!("{rate}k");
        let bufsize = format!("{}k", rate * 2);
        args.extend(os_args(&[
            "-vf",
            &scale,
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            params.preset.as_str(),
            "-crf",
            "23",
            "-maxrate",
            &maxrate,
            "-bufsize",
            &bufsize,
            "-force_key_frames",
            "expr:gte(t,n_forced*4)",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
        ]));
    } else {
        args.extend(os_args(&["-c", "copy"]));
    }
    args.extend(os_args(&[
        "-hls_time",
        "4",
        "-hls_segment_type",
        "fmp4",
        "-hls_flags",
        "single_file+independent_segments",
        "-hls_playlist_type",
        "vod",
        "-y",
    ]));
    args.push(playlist_path.as_os_str().into());
    args
}

/// Encode before HLS while AAC priming and composition timestamps still exist.
/// Completed HLS cannot safely reconstruct the lost encoder-delay metadata.
async fn package_progressive(
    config: &Config,
    processed: &ProcessedAv,
    stem: &str,
    params: &TranscodeParams,
    target_height: i32,
    encoder_args: &[std::ffi::OsString],
) -> Result<PackagedProgressive, ApiError> {
    let progressive_path = processed
        .scratch
        .path()
        .join(format!("progressive{target_height}.mp4"));
    let mut mp4_args = encoder_args.to_vec();
    mp4_args.extend(os_args(&["-movflags", "faststart", "-y"]));
    mp4_args.push(progressive_path.as_os_str().into());
    run_ffmpeg(
        config,
        mp4_args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await?;
    let size_bytes = checked_output_size(&progressive_path, params).await?;
    let metadata = probe(config, &progressive_path).await?;
    if metadata.video_codec.as_deref() != Some("h264")
        || metadata.audio_codec.as_deref().is_some_and(|c| c != "aac")
        || metadata.duration.is_none_or(|d| !d.is_finite() || d <= 0.0)
    {
        return Err(transcode_failed(
            "Progressive rendition has invalid codecs or duration".into(),
        ));
    }
    let digest = crate::video_offload::protocol::digest(&progressive_path).await?;
    let value = plamenu_db::native_video_hls::Progressive {
        file_name: format!("{stem}.draft.progressive{target_height}.mp4"),
        size_bytes,
        sha256: digest.sha256,
        width: metadata
            .width
            .and_then(|w| i32::try_from(w).ok())
            .unwrap_or(0),
        height: metadata
            .height
            .and_then(|h| i32::try_from(h).ok())
            .unwrap_or(0),
        duration: metadata.duration.unwrap_or_default(),
        frame_rate: metadata.frame_rate,
        bitrate: metadata.bitrate.unwrap_or(0),
        audio_codec: metadata.audio_codec,
    };
    if !value.valid() {
        return Err(transcode_failed("Invalid progressive metadata".into()));
    }
    Ok(PackagedProgressive {
        file_path: progressive_path,
        metadata: value,
    })
}

async fn progressive_hls_input(
    config: &Config,
    processed: &ProcessedAv,
    stem: &str,
    params: &TranscodeParams,
    target_height: i32,
    args: &[std::ffi::OsString],
) -> Result<(Vec<std::ffi::OsString>, PackagedProgressive), ApiError> {
    let boundary = args
        .iter()
        .position(|a| a == "-hls_time")
        .expect("native HLS mux boundary");
    let progressive = package_progressive(
        config,
        processed,
        stem,
        params,
        target_height,
        &args[..boundary],
    )
    .await?;
    let mux = args[boundary..].to_vec();
    let mut args = os_args(&["-i"]);
    args.push(progressive.file_path.as_os_str().into());
    args.extend(os_args(&[
        "-map",
        "0:v:0",
        "-map",
        "0:a:0?",
        "-map_metadata",
        "-1",
        "-c",
        "copy",
    ]));
    args.extend(mux);
    Ok((args, progressive))
}

async fn package_one_hls(
    config: &Config,
    processed: &ProcessedAv,
    stem: &str,
    params: &TranscodeParams,
    target_height: i32,
    encode_height: Option<i32>,
    keep_progressive: bool,
) -> Result<PackagedHls, ApiError> {
    let base = format!("hls{target_height}");
    let playlist_path = processed.scratch.path().join(format!("{base}.m3u8"));
    let file_path = processed.scratch.path().join(format!("{base}.m4s"));
    let mut args = native_hls_args(processed, &playlist_path, params, encode_height);
    // Copy-only packaging is I/O-bound; each lower encode takes one heavy slot.
    let _permit = if encode_height.is_some() {
        Some(crate::media_gate::acquire().await)
    } else {
        None
    };
    let progressive = if keep_progressive {
        let (mux, progressive) =
            progressive_hls_input(config, processed, stem, params, target_height, &args).await?;
        args = mux;
        Some(progressive)
    } else {
        None
    };
    run_ffmpeg(
        config,
        args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await?;
    let file_size = checked_output_size(&file_path, params).await?;
    let probe = probe(config, &file_path).await?;
    let (Some(width), Some(height)) = (probe.width, probe.height) else {
        return Err(transcode_failed("HLS rendition has no dimensions".into()));
    };
    let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
        return Err(transcode_failed("HLS rendition dimensions overflow".into()));
    };
    let playlist_len = tokio::fs::metadata(&playlist_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
        .len();
    if playlist_len > 4 * 1024 * 1024 {
        return Err(transcode_failed(
            "HLS playlist exceeds metadata limit".into(),
        ));
    }
    let playlist_bytes = tokio::fs::read(&playlist_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if playlist_bytes.len() > 4 * 1024 * 1024 {
        return Err(transcode_failed(
            "HLS playlist exceeds metadata limit".into(),
        ));
    }
    let playlist = String::from_utf8(playlist_bytes)
        .map_err(|e| transcode_failed(format!("HLS playlist is not UTF-8: {e}")))?;
    let local_name = format!("{base}.m4s");
    if !playlist.starts_with("#EXTM3U\n")
        || !playlist.contains("#EXT-X-ENDLIST")
        || !playlist.contains(&local_name)
        || playlist
            .lines()
            .any(|line| !line.starts_with('#') && line != local_name)
    {
        return Err(transcode_failed(
            "HLS packaging produced an unsafe playlist".into(),
        ));
    }
    let file_name = format!("{stem}.draft.hls{height}.m4s");
    let bandwidth = hls_peak_bandwidth(&playlist)
        .ok_or_else(|| transcode_failed("HLS playlist has no valid segments".into()))?;
    let hash_path = file_path.clone();
    let hash_playlist = playlist.clone();
    let hash_file_name = file_name.clone();
    let segment_sha256 = tokio::task::spawn_blocking(move || {
        native_segment_hashes(&hash_path, &hash_playlist, &hash_file_name, file_size)
    })
    .await
    .map_err(|e| transcode_failed(format!("HLS hash task failed: {e}")))?
    .map_err(transcode_failed)?;
    let playlist = playlist.replace(&local_name, &format!("/media/{file_name}"));
    Ok(PackagedHls {
        file_path,
        file_name,
        file_size,
        width,
        height,
        bandwidth,
        playlist,
        segment_sha256,
        progressive,
    })
}

/// Hash exactly the initialization and media byte ranges written by ffmpeg.
/// Streaming each range in a small buffer prevents a long, high-bitrate video
/// from growing the worker's memory footprint with its segments.
pub(crate) fn native_segment_hashes(
    path: &Path,
    playlist: &str,
    file_name: &str,
    file_size: i64,
) -> Result<serde_json::Value, String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hashes = serde_json::Map::new();
    let mut next_offset = 0_u64;
    let total = u64::try_from(file_size).map_err(|e| e.to_string())?;
    let mut chunk = vec![0_u8; 64 * 1024];
    for line in playlist.lines() {
        let raw = if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let marker = "BYTERANGE=\"";
            attributes
                .split(marker)
                .nth(1)
                .and_then(|tail| tail.split('"').next())
        } else {
            line.strip_prefix("#EXT-X-BYTERANGE:")
        };
        let Some(raw) = raw else { continue };
        let (length, start) = if let Some((length, start)) = raw.split_once('@') {
            (
                length.parse::<u64>().map_err(|e| e.to_string())?,
                start.parse::<u64>().map_err(|e| e.to_string())?,
            )
        } else {
            (raw.parse::<u64>().map_err(|e| e.to_string())?, next_offset)
        };
        if length == 0 || start.checked_add(length).is_none_or(|end| end > total) {
            return Err("HLS byte range exceeds the rendition".into());
        }
        if length > 64 * 1024 * 1024 {
            // Sharing is optional. Very large segments remain ordinary HLS;
            // they must not cause an otherwise valid video to fail processing.
            return Ok(serde_json::json!({}));
        }
        next_offset = start + length;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| e.to_string())?;
        let mut remaining = length;
        let mut digest = Sha256::new();
        while remaining > 0 {
            let amount = usize::try_from(remaining.min(chunk.len() as u64)).unwrap_or(chunk.len());
            file.read_exact(&mut chunk[..amount])
                .map_err(|e| e.to_string())?;
            digest.update(&chunk[..amount]);
            remaining -= amount as u64;
        }
        let mut hex = String::with_capacity(64);
        for byte in digest.finalize() {
            use std::fmt::Write as _;
            write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        hashes.insert(
            format!("{start}-{}", next_offset - 1),
            serde_json::json!(hex),
        );
        if hashes.len() > 10_000 {
            return Ok(serde_json::json!({}));
        }
    }
    if hashes.len() < 2 {
        return Ok(serde_json::json!({}));
    }
    let mut document = serde_json::Map::new();
    document.insert(file_name.to_owned(), serde_json::Value::Object(hashes));
    let document = serde_json::Value::Object(document);
    // There are at most six native renditions. Keep their combined document
    // comfortably below the client's 2 MiB fetch ceiling.
    if document.to_string().len() > 300_000 {
        return Ok(serde_json::json!({}));
    }
    Ok(document)
}

/// HLS BANDWIDTH describes peak segment rate, not whole-file average.
fn hls_peak_bandwidth(playlist: &str) -> Option<i64> {
    let mut duration = None;
    let mut peak = 0;
    for line in playlist.lines() {
        if let Some(raw) = line.strip_prefix("#EXTINF:") {
            duration = raw.split(',').next()?.parse::<f64>().ok();
        } else if let Some(raw) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let seconds = duration.take()?;
            let bytes = raw.split('@').next()?.parse::<u64>().ok()?;
            if !seconds.is_finite() || seconds <= 0.0 || bytes == 0 {
                return None;
            }
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let bitrate = ((bytes as f64 * 8.0 / seconds).ceil().min(i64::MAX as f64)) as i64;
            peak = peak.max(bitrate);
        }
    }
    (peak > 0).then_some(peak)
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
        transcode_audio(config, video_input, &output, params).await?;
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
    run_ffmpeg(
        config,
        args,
        ENCODE_TIMEOUT,
        params.file_size_limit.as_ref(),
    )
    .await?;

    let out_probe = probe(config, &output).await?;
    let (small, blurhash) = extract_poster(config, work_dir, &output, params).await?;
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
    run_ffmpeg(config, args, ENCODE_TIMEOUT, None).await?;
    tokio::fs::read(&output)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

/// Scratch space under the media root, like the on-demand A/V lane: the
/// output moves into the store by rename, and a large intermediate never
/// lands in a RAM-backed `/tmp`.
pub(crate) async fn scratch_dir(config: &Config) -> Result<tempfile::TempDir, ApiError> {
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
    process_av_in_dir(config, &input, dir, params, None).await
}

/// Process an already-spooled upload without reading the original into RAM
/// or making a second full-file copy. The caller retains ownership of input;
/// output and intermediates belong to the returned scratch guard.
pub async fn process_av_file(
    config: &Config,
    input: &Path,
    params: &TranscodeParams,
) -> Result<ProcessedAv, ApiError> {
    process_av_file_mode(config, input, params, None).await
}

/// The native Video composer also accepts sound-only files, publishing an
/// accessible waveform film instead of silently turning them into a separate
/// Audio attachment. Ordinary attachment uploads retain their Audio semantics.
pub async fn process_native_video_file(
    config: &Config,
    input: &Path,
    params: &TranscodeParams,
    poster_time_seconds: f64,
) -> Result<ProcessedAv, ApiError> {
    process_av_file_mode(config, input, params, Some(poster_time_seconds)).await
}

/// Process a finite closed prefix through a held native descriptor. Every
/// probe/encode reads that bounded view; source bytes are never copied or
/// truncated. Only native replay finalization constructs this input.
#[cfg(target_os = "linux")]
pub async fn process_native_video_prefix(
    config: &Config,
    input: &Path,
    bytes: u64,
    params: &TranscodeParams,
) -> Result<ProcessedAv, ApiError> {
    use std::os::fd::AsRawFd;
    let source = std::fs::File::open(input).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let metadata = source
        .metadata()
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    if !metadata.is_file()
        || bytes == 0
        || bytes > metadata.len()
        || metadata.len() > params.max_av_bytes as u64
    {
        return Err(unsupported());
    }
    let view = std::path::PathBuf::from(format!(
        "subfile,,start,0,end,{bytes},,:/proc/{}/fd/{}",
        std::process::id(),
        source.as_raw_fd()
    ));
    let processed =
        process_av_in_dir(config, &view, scratch_dir(config).await?, params, Some(0.0)).await;
    drop(source);
    processed
}

async fn process_av_file_mode(
    config: &Config,
    input: &Path,
    params: &TranscodeParams,
    native_poster_time: Option<f64>,
) -> Result<ProcessedAv, ApiError> {
    let metadata = tokio::fs::metadata(input)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    if !metadata.is_file()
        || metadata.len() > u64::try_from(params.max_av_bytes).unwrap_or(u64::MAX)
    {
        return Err(ApiError::Unprocessable(
            "Validation failed: File size exceeds the limit".into(),
        ));
    }
    process_av_in_dir(
        config,
        input,
        scratch_dir(config).await?,
        params,
        native_poster_time,
    )
    .await
}

async fn process_av_in_dir(
    config: &Config,
    input: &Path,
    dir: tempfile::TempDir,
    params: &TranscodeParams,
    native_poster_time: Option<f64>,
) -> Result<ProcessedAv, ApiError> {
    let probed = probe(config, input).await?;
    if let Some(window) = params.window {
        #[expect(
            clippy::cast_precision_loss,
            reason = "checked seven-day millisecond bound"
        )]
        let end_seconds = window.end_ms() as f64 / 1000.0;
        if native_poster_time.is_none()
            || probed
                .duration
                .is_none_or(|duration| !duration.is_finite() || end_seconds > duration + 0.001)
        {
            return Err(ApiError::Unprocessable(
                "The trim window exceeds the retained native source".into(),
            ));
        }
    }
    let declared = classify(&probed, params.gifv_max_seconds)?;

    match declared {
        AvKind::Video | AvKind::Gifv => {
            let output = dir.path().join("out.mp4");
            transcode_video(config, &probed, input, &output, params).await?;
            let file_size = checked_output_size(&output, params).await?;
            // `meta.original` reflects the delivered file, like Mastodon's
            // `populate_meta` probing `queued_for_write`.
            let out_probe = probe(config, &output).await?;

            let seek = native_poster_time
                .map_or(0.0, |time| poster_seek_seconds(time, out_probe.duration));
            let (small, blurhash) =
                extract_poster_at(config, dir.path(), &output, params, seek).await?;

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
        AvKind::Audio if native_poster_time.is_some() => {
            let output = dir.path().join("out.mp4");
            transcode_audio_visualization(config, input, &output, params).await?;
            let file_size = checked_output_size(&output, params).await?;
            let out_probe = probe(config, &output).await?;
            let seek = native_poster_time
                .map_or(0.0, |time| poster_seek_seconds(time, out_probe.duration));
            let (small, blurhash) =
                extract_poster_at(config, dir.path(), &output, params, seek).await?;
            Ok(ProcessedAv {
                file_path: output,
                file_size,
                scratch: dir,
                content_type: "video/mp4",
                extension: "mp4",
                kind: "video",
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
            transcode_audio(config, input, &output, params).await?;
            let out_probe = probe(config, &output).await?;
            let file_size = checked_output_size(&output, params).await?;
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

async fn checked_output_size(path: &Path, params: &TranscodeParams) -> Result<i64, ApiError> {
    let bytes = file_len(path).await?;
    if params
        .file_size_limit
        .as_ref()
        .is_some_and(|limit| u64::try_from(bytes).unwrap_or(u64::MAX) >= limit.bytes)
    {
        return Err(transcode_failed(
            "encoded output reached its file-size limit".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressive_policy_prefers_existing_720_target_and_measured_original() {
        let ladder = [
            (1080, Some(6000), Some(1000)),
            (720, Some(3000), None),
            (480, Some(1800), None),
            (360, Some(1000), None),
        ];
        assert_eq!(progressive_choice(&ladder, 1000), Some(1));
        assert_eq!(
            progressive_choice(&[(720, Some(4000), None), (480, Some(1800), None)], 1000),
            Some(0)
        );
        assert_eq!(
            progressive_choice(&[(720, Some(4001), None), (480, Some(1800), None)], 1000),
            Some(1)
        );
        assert_eq!(
            progressive_choice(
                &[(720, Some(3999), Some(1000)), (480, Some(1800), None)],
                1000
            ),
            Some(0)
        );
        assert_eq!(
            progressive_choice(&[(720, None, Some(1000)), (480, Some(1800), None)], 1000),
            Some(1)
        );
        assert_eq!(
            progressive_choice(&[(1080, None, Some(500)), (2160, None, Some(900))], 1000),
            Some(0)
        );
        assert_eq!(
            progressive_choice(&[(1080, None, Some(1000)), (2160, None, None)], 1000),
            None
        );
        assert_eq!(progressive_choice(&[(0, Some(1), Some(1))], 1000), None);
        assert_eq!(
            progressive_choice(&[(720, Some(0), Some(1000))], 1000),
            None
        );
    }

    #[test]
    fn native_ladder_is_finite_downscale_only_and_avoids_redundant_rungs() {
        for (source, expected) in [
            (2160, vec![1080, 720, 480, 360, 240]),
            (1080, vec![720, 480, 360, 240]),
            (720, vec![480, 360, 240]),
            (480, vec![360, 240]),
            (360, vec![240]),
            (250, vec![]),
            (240, vec![]),
            (0, vec![]),
        ] {
            assert_eq!(native_lower_heights(source).collect::<Vec<_>>(), expected);
        }
        assert_eq!(
            NATIVE_HLS_MAX_RENDITIONS,
            NATIVE_HLS_HEIGHTS.len() as u64 + 1
        );
    }

    #[test]
    fn native_hls_hashes_exact_init_and_media_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.m4s");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let playlist = "#EXTM3U\n#EXT-X-MAP:URI=\"stream.m4s\",BYTERANGE=\"4@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:4@4\nstream.m4s\n#EXT-X-ENDLIST\n";
        let hashes = native_segment_hashes(&path, playlist, "published.m4s", 8).unwrap();
        assert_eq!(
            hashes["published.m4s"]["0-3"],
            "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589"
        );
        assert_eq!(
            hashes["published.m4s"]["4-7"],
            "e5e088a0b66163a0a26a5e053d2a4496dc16ab6e0e3dd1adf2d16aa84a078c9d"
        );
        assert!(native_segment_hashes(&path, playlist, "published.m4s", 7).is_err());
        let oversized = "#EXTM3U\n#EXT-X-MAP:URI=\"stream.m4s\",BYTERANGE=\"4@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:67108865@4\nstream.m4s\n#EXT-X-ENDLIST\n";
        assert_eq!(
            native_segment_hashes(&path, oversized, "published.m4s", 67_108_869).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn hls_bandwidth_uses_peak_segment_not_file_average() {
        let playlist = "#EXTM3U\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:400@0\nstream.m4s\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:600@400\nstream.m4s\n#EXT-X-ENDLIST\n";
        assert_eq!(hls_peak_bandwidth(playlist), Some(2_400));
        assert_eq!(hls_peak_bandwidth("#EXTM3U\n#EXT-X-ENDLIST\n"), None);
        assert_eq!(
            hls_peak_bandwidth("#EXTINF:0,\n#EXT-X-BYTERANGE:1@0\n"),
            None
        );
    }

    #[test]
    fn every_local_input_gets_its_own_protocol_and_demuxer_boundary() {
        let args = restrict_local_inputs(os_args(&[
            "-i",
            "video.mp4",
            "-i",
            "audio.m4a",
            "-c",
            "copy",
            "out.mp4",
        ]));
        let options = local_input_options();
        let mut expected = options.clone();
        expected.extend(os_args(&["-i", "video.mp4"]));
        expected.extend(options);
        expected.extend(os_args(&["-i", "audio.m4a", "-c", "copy", "out.mp4"]));
        assert_eq!(args, expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prefix_protocol_requires_our_held_regular_descriptor_and_finite_end() {
        use std::os::fd::AsRawFd;
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"finite prefix").unwrap();
        let valid = format!(
            "subfile,,start,0,end,6,,:/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        );
        assert!(trusted_prefix_input(Path::new(&valid)));
        assert_eq!(
            local_input_options_for(Path::new(&valid))[1],
            "file,subfile"
        );
        for input in [
            "subfile,,start,0,end,6,,:/etc/passwd".to_owned(),
            valid.replace("end,6,", "end,0,"),
            valid.replace("end,6,", "end,999999,"),
            valid.replace("/fd/", "/fd/../"),
            valid.replace("end,6,", "end,+6,"),
        ] {
            assert!(!trusted_prefix_input(Path::new(&input)));
            assert_eq!(local_input_options_for(Path::new(&input))[1], "file");
        }
    }

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

    #[test]
    fn poster_command_emits_avif_and_small_blurhash_source_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("source.mp4");
        let poster = dir.path().join("poster.avif");
        let hash = dir.path().join("hash.pam");
        let generated = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=800x450:duration=0.1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&input)
            .status()
            .expect("the required ffmpeg is installed");
        assert!(generated.success());

        let args = poster_ffmpeg_args(&input, &poster, &hash, &TranscodeParams::default(), 0.0);
        let extracted = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "fatal"])
            .args(args)
            .status()
            .expect("the required ffmpeg is installed");
        assert!(extracted.success());
        let poster_bytes = std::fs::read(poster).unwrap();
        assert_eq!(avif_dimensions(&poster_bytes), Some((640, 360)));
        let hash_image = read_pam(&hash).unwrap();
        assert_eq!((hash_image.width(), hash_image.height()), (64, 36));
        assert!(blurhash_for(&hash_image).is_some());
    }

    #[test]
    fn poster_seek_stays_inside_the_stream() {
        for (requested, duration, expected) in [
            (3.0, Some(5.0), 3.0),
            (99.0, Some(5.0), 4.0),
            (-1.0, Some(5.0), 0.0),
            (f64::NAN, Some(5.0), 0.0),
            (1.0, None, 0.0),
        ] {
            assert!((poster_seek_seconds(requested, duration) - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn selected_poster_frame_is_not_always_the_first_frame() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("two-colours.mp4");
        let encoded = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=64x64:rate=5:duration=2",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=64x64:rate=5:duration=2",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0[v]",
                "-map",
                "[v]",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(encoded.success());
        for (time, color) in [(0.0, "red"), (3.0, "blue")] {
            let poster = dir.path().join(format!("{color}.avif"));
            let hash = dir.path().join(format!("{color}.pam"));
            let extracted = std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error"])
                .args(poster_ffmpeg_args(
                    &input,
                    &poster,
                    &hash,
                    &TranscodeParams::default(),
                    time,
                ))
                .status()
                .unwrap();
            assert!(extracted.success());
            let pixel = std::process::Command::new("ffmpeg")
                .args(["-nostdin", "-loglevel", "error", "-i"])
                .arg(poster)
                .args([
                    "-vf",
                    "scale=1:1",
                    "-pix_fmt",
                    "rgb24",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(pixel.status.success());
            assert_eq!(pixel.stdout.len(), 3);
            if color == "red" {
                assert!(i16::from(pixel.stdout[0]) > i16::from(pixel.stdout[2]) + 100);
            } else {
                assert!(i16::from(pixel.stdout[2]) > i16::from(pixel.stdout[0]) + 100);
            }
        }
    }
}
