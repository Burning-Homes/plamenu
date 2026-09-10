//! Image processing for uploads and incoming media caches. **Local** avatars
//! and headers are always decoded and re-encoded to a Mastodon-compatible
//! photo format (JPEG, or PNG when the image has alpha) — Mastodon rejects
//! AVIF for avatars/headers, so those must never ship as AVIF. Status uploads
//! are governed by operator settings ([`FullMedia`] / [`PreviewMedia`]): the
//! full rendition defaults to `passthrough` (stored as received, only
//! metadata stripped) and the preview defaults to AVIF; cached remote
//! attachments have their own full-rendition knob, defaulting to AVIF.
//! Cached copies of **incoming** emoji, avatars/headers and preview-card
//! images ([`CachedImage`]) default to AVIF — they serve local clients only
//! and never appear in outbound federation — except animated ones, which are
//! always kept as arrived. Re-encoding (or the passthrough metadata strip)
//! removes EXIF/GPS for privacy, matching Mastodon. Status uploads also get a
//! downscaled `small` style (Mastodon's preview) and a blurhash.

use image::{AnimationDecoder, DynamicImage, GenericImageView, ImageFormat};

use crate::error::ApiError;

/// How the full/original rendition of a **status image upload** is produced.
/// Operator-configurable (`media_full_processing`); avatars/headers ignore this
/// and always use the photo path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullMedia {
    /// Store as received: original format and dimensions, only metadata
    /// (EXIF/GPS/XMP) stripped losslessly. The default.
    Passthrough,
    /// Downscale to the configured edge and re-encode as AVIF (much smaller).
    Avif,
    /// Downscale and re-encode as JPEG (PNG when the image has alpha).
    Jpeg,
    /// Downscale and re-encode as JPEG XL via ffmpeg's libjxl (opt-in:
    /// limited client support in 2026). Falls back to passthrough when the
    /// encode fails (e.g. an ffmpeg without libjxl).
    Jxl,
}

/// How the downscaled `small`/preview rendition of a status upload is encoded.
/// Operator-configurable (`media_preview_processing`). AVIF is Mastodon-safe
/// here — its `IMAGE_MIME_TYPES` (used for attachment thumbnails) includes
/// `image/avif`, unlike the avatar/header lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewMedia {
    /// AVIF at the cheaper preview speed — the default.
    Avif,
    /// JPEG (PNG when the image has alpha).
    Jpeg,
}

impl FullMedia {
    /// Parses the stored `media_full_processing` value, falling back to the
    /// default (`passthrough`) for anything unrecognised.
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value {
            "avif" => Self::Avif,
            "jpeg" => Self::Jpeg,
            "jxl" => Self::Jxl,
            _ => Self::Passthrough,
        }
    }
}

impl PreviewMedia {
    /// Parses the stored `media_preview_processing` value, falling back to the
    /// default (`avif`).
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value {
            "jpeg" => Self::Jpeg,
            _ => Self::Avif,
        }
    }
}

/// How cached copies of incoming emoji, avatar/header and preview-card images
/// are stored (`media_cached_image_processing`). These renditions are served
/// to local clients only — nothing in outbound federation ever points at them
/// — so unlike *local* avatars/headers they are safe to re-encode as AVIF.
/// Animated images are always kept as arrived regardless: re-encoding through
/// the still pipeline would freeze them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachedImage {
    /// Downscale still images to the slot's edge and re-encode as AVIF — the
    /// default.
    Avif,
    /// Store every image as it arrived.
    Passthrough,
}

impl CachedImage {
    /// Parses the stored `media_cached_image_processing` value, falling back
    /// to the default (`avif`).
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value {
            "passthrough" => Self::Passthrough,
            _ => Self::Avif,
        }
    }
}

/// Image uploads above this byte size are rejected outright (Mastodon's
/// `IMAGE_LIMIT`).
pub const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Video/audio uploads get Mastodon's larger `VIDEO_LIMIT`.
pub const MAX_AV_UPLOAD_BYTES: usize = 99 * 1024 * 1024;
/// Mastodon's `image_matrix_limit`: reject absurdly large pixel counts.
const MAX_PIXELS: u64 = 33_177_600;
/// Larger images are downscaled so their longest edge fits this.
pub const MAX_EDGE: u32 = 1920;
/// Mastodon-like bounds for profile images.
pub const AVATAR_MAX_EDGE: u32 = 400;
pub const HEADER_MAX_EDGE: u32 = 1500;
/// Cached remote emoji are rendered inline (~1.2em) and in reaction bars;
/// this leaves ~10x headroom for zooming without keeping origin-sized files.
pub const EMOJI_MAX_EDGE: u32 = 256;
/// Cached preview-card images: og:image sources are commonly 1200×630.
pub const CARD_MAX_EDGE: u32 = 1200;
/// Area cap of the `small` (preview) style — Mastodon's 640x360.
const SMALL_MAX_PIXELS: u32 = 230_400;
/// Blurhashes use Mastodon's 4x4 components.
const BLURHASH_COMPONENTS: u32 = 4;
/// Blurhash encoding cost scales with pixels; a thumbnail this size is
/// visually identical after the blur.
const BLURHASH_MAX_EDGE: u32 = 64;

/// Bytes of randomness in an attachment's storage file name.
const STEM_ENTROPY_BYTES: usize = 16;

/// The storage file stem for one attachment: the media row's id followed by
/// [`STEM_ENTROPY_BYTES`] of randomness, hex-encoded.
///
/// `/media/{file}` is public and unauthenticated by necessity — remote servers
/// and their users fetch our attachments without credentials, exactly as
/// Mastodon's do — so for a followers-only or direct post the **file name is
/// the only thing between the attachment and anyone who asks for it**.
///
/// A bare snowflake id is not that. Ids are `unix_millis << 16 | counter`,
/// they come from one process-wide counter shared with every public status
/// id, and they increase strictly — so an attacker who sees two public status
/// ids bracketing the upload knows the media id lies in a short range and can
/// simply walk it against the handful of extensions we emit. The random
/// suffix is what makes the name unguessable, matching Mastodon's random
/// per-file names. The id prefix is kept so stored files still sort and group
/// by upload time for operators, and reveals nothing an API response doesn't.
///
/// Only `[0-9a-f]`, so the name stays inside the strict character class
/// `routes::media::serve` enforces.
///
/// Every write mints its own stem and records it on the row; nothing
/// re-derives a name from an id, so a re-processed attachment simply gets a
/// fresh name (the previous file becomes an orphan the media-cleanup
/// reconciliation sweep collects).
#[must_use]
pub fn storage_stem(media_id: i64) -> String {
    let mut bytes = [0u8; STEM_ENTROPY_BYTES];
    getrandom::fill(&mut bytes).expect("OS entropy source failed");
    let mut stem = media_id.to_string();
    for byte in bytes {
        use std::fmt::Write as _;
        write!(stem, "{byte:02x}").expect("writing to a String cannot fail");
    }
    stem
}

/// Content type of a stored media file, by the extension we generated.
#[must_use]
pub fn content_type_for(file_name: &str) -> &'static str {
    match file_name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("avif") => "image/avif",
        Some("jxl") => "image/jxl",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("md") => "text/markdown",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Plamenu's default custom-emoji upload size cap.
pub const DEFAULT_MAX_EMOJI_BYTES: usize = 512 * 1024;
/// Absolute request/validation ceiling for the operator-configurable custom
/// emoji limit. Keeping this finite lets the multipart routes reject abusive
/// bodies before buffering them in memory.
pub const HARD_MAX_EMOJI_BYTES: usize = 16 * 1024 * 1024;

/// Validates an uploaded custom-emoji image: Mastodon's accepted formats
/// (PNG, GIF, WebP) and the installation's configured byte cap. Returns
/// `(content_type, extension)`.
/// The bytes are stored as-is — re-encoding would strip GIF/WebP animation,
/// and the upload is the operator's own deliberate choice, not arbitrary
/// user content.
pub fn validate_emoji_image(
    input: &[u8],
    configured_max_bytes: usize,
) -> Result<(&'static str, &'static str), ApiError> {
    let max_bytes = configured_max_bytes.min(HARD_MAX_EMOJI_BYTES);
    if input.len() > max_bytes {
        let actual_kib = input.len().div_ceil(1024);
        let max_kib = max_bytes / 1024;
        return Err(ApiError::Unprocessable(format!(
            "Validation failed: the file is {actual_kib} KiB, but this server accepts custom emoji files up to {max_kib} KiB. Choose a smaller file or ask an administrator to raise the limit."
        )));
    }
    match image::guess_format(input) {
        Ok(ImageFormat::Png) => Ok(("image/png", "png")),
        Ok(ImageFormat::Gif) => Ok(("image/gif", "gif")),
        Ok(ImageFormat::WebP) => Ok(("image/webp", "webp")),
        _ => Err(unsupported(
            "the file is not a supported PNG, GIF, or WebP image",
        )),
    }
}

/// The downscaled preview rendition of a processed image.
pub struct SmallImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The preview's own file extension — it can differ from the full
    /// rendition's (e.g. an AVIF preview of a passthrough PNG original), so the
    /// storage layer must name the `.small.*` file with *this*, not the
    /// original's extension.
    pub extension: &'static str,
}

pub struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub extension: &'static str,
    pub width: u32,
    pub height: u32,
    /// Only produced for status uploads (not avatars/headers).
    pub small: Option<SmallImage>,
    pub blurhash: Option<String>,
}

fn unsupported(reason: &str) -> ApiError {
    ApiError::Unprocessable(format!("Validation failed: {reason}"))
}

/// True when the bytes are an image we process in-request (the still-image
/// pipeline): a JPEG/PNG/WebP, or a non-animated GIF. Animated GIFs and
/// non-images go through ffmpeg (the AV path, where gifv is detected). Used by
/// both the upload endpoint and the remote-media download worker.
#[must_use]
pub fn is_still_image(input: &[u8]) -> bool {
    if is_jxl(input) {
        return true;
    }
    match image::guess_format(input) {
        Ok(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP) => true,
        Ok(ImageFormat::Gif) => !is_animated_gif(input),
        _ => false,
    }
}

/// True when the bytes are JPEG XL: the bare codestream signature or the
/// ISOBMFF container signature. `image::guess_format` does not know JXL.
#[must_use]
pub fn is_jxl(input: &[u8]) -> bool {
    input.starts_with(&[0xFF, 0x0A])
        || input.starts_with(&[
            0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
        ])
}

/// Decodes a JPEG XL image (first frame) via the pure-Rust `jxl-oxide` —
/// the decode half of the optional JXL support; encoding goes through
/// ffmpeg's libjxl. `None` for unparseable input or over-limit dimensions
/// (checked from the headers, before any pixel is decoded).
fn decode_jxl(input: &[u8]) -> Option<DynamicImage> {
    let image = jxl_oxide::JxlImage::builder()
        .read(std::io::Cursor::new(input))
        .ok()?;
    if u64::from(image.width()) * u64::from(image.height()) > MAX_PIXELS {
        return None;
    }
    let render = image.render_frame(0).ok()?;
    let frame = render.image_all_channels();
    let (width, height, channels) = (frame.width(), frame.height(), frame.channels());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let samples: Vec<u8> = frame
        .buf()
        .iter()
        .map(|s| (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8)
        .collect();
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    match channels {
        1 => image::GrayImage::from_raw(width, height, samples).map(DynamicImage::ImageLuma8),
        3 => image::RgbImage::from_raw(width, height, samples).map(DynamicImage::ImageRgb8),
        4 => image::RgbaImage::from_raw(width, height, samples).map(DynamicImage::ImageRgba8),
        _ => None,
    }
}

/// True when the bytes are a multi-frame GIF — those transcode to `gifv`
/// (an mp4) like Mastodon; still GIFs go through the image pipeline.
#[must_use]
pub fn is_animated_gif(input: &[u8]) -> bool {
    let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(input)) else {
        return false;
    };
    decoder.into_frames().take(2).flatten().count() > 1
}

/// True when a GIF's first frame carries any transparent pixel — the signal
/// that converting it to H.264 gifv (yuv420p, no alpha channel) would render
/// its transparent regions as a solid background. Frames after the first can
/// legitimately use transparency for delta compression, so only the first
/// frame (composited onto nothing) is meaningful here.
#[must_use]
pub fn gif_first_frame_has_alpha(input: &[u8]) -> bool {
    let Ok(decoder) = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(input)) else {
        return false;
    };
    let Some(Ok(frame)) = decoder.into_frames().next() else {
        return false;
    };
    frame.buffer().pixels().any(|pixel| pixel[3] < u8::MAX)
}

/// True when the bytes are an animated image in any container we accept:
/// a multi-frame GIF, an APNG, or an animated WebP. These must never go
/// through the still-image pipeline — decoding takes the first frame only,
/// silently freezing the animation.
#[must_use]
pub fn is_animated_image(input: &[u8]) -> bool {
    match image::guess_format(input) {
        Ok(ImageFormat::Gif) => is_animated_gif(input),
        Ok(ImageFormat::Png) => is_apng(input),
        Ok(ImageFormat::WebP) => is_animated_webp(input),
        _ => false,
    }
}

/// True when a PNG is an APNG: the spec requires the `acTL` (animation
/// control) chunk before the first `IDAT`, so a bounded chunk walk finds it.
/// `image::guess_format` cannot tell the two apart.
fn is_apng(input: &[u8]) -> bool {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if input.get(0..8) != Some(&SIG) {
        return false;
    }
    let mut i = 8;
    while let (Some(len), Some(kind)) = (input.get(i..i + 4), input.get(i + 4..i + 8)) {
        match kind {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => {}
        }
        let len = u32::from_be_bytes(len.try_into().unwrap_or_default()) as usize;
        i += 12 + len; // length(4) + type(4) + data + crc(4)
    }
    false
}

/// True when a RIFF/WebP container is animated: the `VP8X` animation flag or
/// an `ANIM`/`ANMF` chunk.
fn is_animated_webp(input: &[u8]) -> bool {
    if input.get(0..4) != Some(b"RIFF".as_slice()) || input.get(8..12) != Some(b"WEBP".as_slice()) {
        return false;
    }
    let mut i = 12;
    while let (Some(fourcc), Some(size)) = (input.get(i..i + 4), input.get(i + 4..i + 8)) {
        match fourcc {
            b"ANIM" | b"ANMF" => return true,
            b"VP8X" if input.get(i + 8).is_some_and(|flags| flags & 0x02 != 0) => {
                return true;
            }
            _ => {}
        }
        let size = u32::from_le_bytes(size.try_into().unwrap_or_default()) as usize;
        i += 8 + size + (size & 1); // chunks are padded to an even length
    }
    false
}

/// Encodes Mastodon's 4x4-component blurhash from a decoded image,
/// downscaling first (encoding cost is per-pixel; the blur hides the loss).
#[must_use]
pub fn blurhash_for(image: &DynamicImage) -> Option<String> {
    let (width, height) = image.dimensions();
    let shrunk = if width.max(height) > BLURHASH_MAX_EDGE {
        image.resize(
            BLURHASH_MAX_EDGE,
            BLURHASH_MAX_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image.clone()
    };
    let rgba = shrunk.to_rgba8();
    blurhash::encode(
        BLURHASH_COMPONENTS,
        BLURHASH_COMPONENTS,
        rgba.width(),
        rgba.height(),
        rgba.as_raw(),
    )
    .ok()
}

/// Default AVIF quality (0–100, higher = better) for re-encoded media.
const AVIF_QUALITY: u8 = 70;
/// rav1e speed (0–10, higher = faster/larger) for full-size renditions. The
/// original may be re-encoded inline on the media proxy's cache-miss path, so
/// speed is chosen to stay well inside `PROXY_FETCH_TIMEOUT` even for a
/// max-edge image (≈2 s for 1920² on the staging box); the extra bytes over a
/// slower speed are immaterial for a rendition loaded one-at-a-time (lightbox).
const AVIF_SPEED_FULL: u8 = 10;
/// The preview is tiny (≤640×360), so a slower speed buys better compression
/// almost for free (~1.5 s) — and it is the feed-critical rendition where bytes
/// matter most.
const AVIF_SPEED_PREVIEW: u8 = 6;

/// The operator's encoder knobs (M39), read from the instance settings once
/// per processing run. [`Default`] matches fresh-install encoder settings
/// for callers without an instance-settings row (tests and site uploads).
#[derive(Clone, Debug)]
pub struct EncodeParams {
    pub avif_quality: u8,
    pub avif_speed_full: u8,
    pub avif_speed_preview: u8,
    pub jpeg_quality: u8,
    /// Longest edge a re-encoded full rendition is downscaled to.
    pub max_edge: u32,
    /// Image-upload byte cap (never above the compiled [`MAX_UPLOAD_BYTES`]).
    pub max_image_bytes: usize,
    /// JPEG XL, encoded through ffmpeg's libjxl.
    pub jxl_distance: f32,
    pub jxl_effort: u8,
    pub ffmpeg_path: String,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self {
            avif_quality: AVIF_QUALITY,
            avif_speed_full: AVIF_SPEED_FULL,
            avif_speed_preview: AVIF_SPEED_PREVIEW,
            jpeg_quality: 85,
            max_edge: MAX_EDGE,
            max_image_bytes: MAX_UPLOAD_BYTES,
            jxl_distance: 1.0,
            jxl_effort: 4,
            ffmpeg_path: "ffmpeg".to_owned(),
        }
    }
}

impl EncodeParams {
    /// Bundles the operator's settings for one processing run. Values are
    /// clamped defensively — the admin form already validates, but the row
    /// is plain integers.
    #[must_use]
    pub fn from_settings(
        settings: &plamenu_db::instance_settings::InstanceSettings,
        ffmpeg_path: &str,
    ) -> Self {
        let clamp_u8 =
            |v: i32, min: u8, max: u8| u8::try_from(v.clamp(min.into(), max.into())).unwrap_or(min);
        Self {
            avif_quality: clamp_u8(settings.media_avif_quality, 1, 100),
            avif_speed_full: clamp_u8(settings.media_avif_speed_full, 1, 10),
            avif_speed_preview: clamp_u8(settings.media_avif_speed_preview, 1, 10),
            jpeg_quality: clamp_u8(settings.media_jpeg_quality, 1, 100),
            max_edge: u32::try_from(settings.media_max_edge.clamp(320, 8192)).unwrap_or(MAX_EDGE),
            max_image_bytes: usize::try_from(settings.media_max_image_mb.clamp(1, 16))
                .unwrap_or(16)
                .saturating_mul(1024 * 1024),
            jxl_distance: settings.media_jxl_distance.clamp(0.0, 15.0),
            jxl_effort: clamp_u8(settings.media_jxl_effort, 1, 9),
            ffmpeg_path: ffmpeg_path.to_owned(),
        }
    }
}

/// How a processed rendition is encoded.
#[derive(Clone, Copy)]
enum Encode<'a> {
    /// AVIF at the given rav1e speed and quality (re-encoded user media).
    Avif { speed: u8, quality: u8 },
    /// JPEG at the given quality, or PNG when the image carries alpha — the
    /// legacy codec kept for operator site uploads, whose favicon/app-icon
    /// styles must stay PNG and whose original is re-decoded downstream (we
    /// cannot decode AVIF).
    Photo { quality: u8 },
    /// JPEG XL through ffmpeg's libjxl (the build cannot link libjxl — C++ —
    /// but the official image's ffmpeg carries the encoder).
    Jxl {
        ffmpeg_path: &'a str,
        distance: f32,
        effort: u8,
    },
}

/// Re-encodes a processed rendition per [`Encode`], returning the bytes plus the
/// chosen content type and file extension.
fn encode_image(
    image: &DynamicImage,
    encode: Encode<'_>,
) -> Result<(Vec<u8>, &'static str, &'static str), ApiError> {
    let mut bytes = Vec::new();
    match encode {
        Encode::Avif { speed, quality } => {
            let encoder = image::codecs::avif::AvifEncoder::new_with_speed_quality(
                &mut bytes, speed, quality,
            );
            image
                .write_with_encoder(encoder)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Ok((bytes, "image/avif", "avif"))
        }
        Encode::Photo { .. } if image.color().has_alpha() => {
            image
                .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Ok((bytes, "image/png", "png"))
        }
        Encode::Photo { quality } => {
            let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
            let mut cursor = std::io::Cursor::new(&mut bytes);
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
            rgb.write_with_encoder(encoder)
                .map_err(|e| ApiError::Internal(Box::new(e)))?;
            Ok((bytes, "image/jpeg", "jpg"))
        }
        Encode::Jxl {
            ffmpeg_path,
            distance,
            effort,
        } => {
            let bytes = encode_jxl(image, ffmpeg_path, distance, effort)?;
            Ok((bytes, "image/jxl", "jxl"))
        }
    }
}

/// Encodes an already-resized rendition as JPEG XL by shelling out to ffmpeg
/// (libjxl). Runs on the blocking pool (the caller is `process_image`), so a
/// blocking wait is fine; a poll-loop watchdog kills a wedged encoder.
fn encode_jxl(
    image: &DynamicImage,
    ffmpeg_path: &str,
    distance: f32,
    effort: u8,
) -> Result<Vec<u8>, ApiError> {
    const JXL_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);
    let dir = tempfile::tempdir().map_err(|e| ApiError::Internal(Box::new(e)))?;
    let input = dir.path().join("in.png");
    let output = dir.path().join("out.jxl");
    image
        .save_with_format(&input, ImageFormat::Png)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let mut child = std::process::Command::new(ffmpeg_path)
        .args(["-nostdin", "-loglevel", "fatal", "-i"])
        .arg(&input)
        .args(["-frames:v", "1", "-c:v", "libjxl", "-distance"])
        .arg(distance.to_string())
        .arg("-effort")
        .arg(effort.to_string())
        .arg("-y")
        .arg(&output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ApiError::Internal(format!("could not run ffmpeg: {e}").into()))?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > JXL_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ApiError::Internal("JPEG XL encode timed out".into()));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => return Err(ApiError::Internal(Box::new(e))),
        }
    };
    if !status.success() {
        return Err(ApiError::Internal(
            "ffmpeg failed to encode JPEG XL (is libjxl available?)".into(),
        ));
    }
    std::fs::read(&output).map_err(|e| ApiError::Internal(Box::new(e)))
}

/// Encodes a preview/poster frame (a video's extracted still) as AVIF, using
/// the same settings as an image's small style. Video posters are opaque and
/// transcoded in the background, so the preview speed applies here too. The AV
/// counterpart of the small style [`process_image`] produces.
pub fn encode_preview_avif(
    frame: &DynamicImage,
    speed: u8,
    quality: u8,
) -> Result<Vec<u8>, ApiError> {
    let (bytes, _, _) = encode_image(frame, Encode::Avif { speed, quality })?;
    Ok(bytes)
}

/// Downscales to the `small` style: Mastodon caps the preview's *area*
/// (230400 px ≙ 640x360), not its edge.
fn small_style(image: &DynamicImage) -> DynamicImage {
    let (width, height) = image.dimensions();
    let area = u64::from(width) * u64::from(height);
    if area <= u64::from(SMALL_MAX_PIXELS) {
        return image.clone();
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let scale = (f64::from(SMALL_MAX_PIXELS) / area as f64).sqrt();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let target_w = ((f64::from(width) * scale).round() as u32).max(1);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let target_h = ((f64::from(height) * scale).round() as u32).max(1);
    image.resize(target_w, target_h, image::imageops::FilterType::Lanczos3)
}

/// Losslessly removes metadata (EXIF/XMP/comments) from an image container,
/// keeping the pixel/animation data and format untouched — the passthrough
/// path's privacy strip, matching Mastodon's "no EXIF/GPS" guarantee without
/// re-encoding. Returns `None` if the container can't be parsed (the caller
/// then re-encodes as a fallback). GIF carries no EXIF/GPS, so it is kept
/// byte-for-byte.
fn strip_metadata(input: &[u8], format: ImageFormat) -> Option<Vec<u8>> {
    match format {
        ImageFormat::Jpeg => strip_jpeg_metadata(input),
        ImageFormat::Png => strip_png_metadata(input),
        ImageFormat::WebP => strip_webp_metadata(input),
        ImageFormat::Gif => Some(input.to_vec()),
        _ => None,
    }
}

/// Drops every APP1 (`Exif`/XMP) and COM segment from a JPEG, copying all other
/// segments plus the entropy-coded scan data verbatim.
fn strip_jpeg_metadata(input: &[u8]) -> Option<Vec<u8>> {
    if input.get(0..2)? != [0xFF, 0xD8] {
        return None; // not SOI
    }
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(&input[0..2]);
    let mut i = 2;
    loop {
        if *input.get(i)? != 0xFF {
            return None;
        }
        // Skip any 0xFF fill bytes before the marker code.
        let mut m = i + 1;
        while *input.get(m)? == 0xFF {
            m += 1;
        }
        let marker = *input.get(m)?;
        i = m + 1;
        match marker {
            // Start of scan: the SOS segment is followed by entropy-coded data
            // with no parseable length, so copy it and everything after it.
            0xDA => {
                out.extend_from_slice(&[0xFF, marker]);
                out.extend_from_slice(input.get(i..)?);
                return Some(out);
            }
            // End of image before any scan (unusual) — finish.
            0xD9 => {
                out.extend_from_slice(&[0xFF, marker]);
                return Some(out);
            }
            // Standalone markers carry no length payload.
            0xD0..=0xD7 | 0x01 => out.extend_from_slice(&[0xFF, marker]),
            // A length-prefixed segment (length includes its own two bytes).
            _ => {
                let len = u16::from_be_bytes([*input.get(i)?, *input.get(i + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                let seg_end = i + len;
                input.get(i..seg_end)?; // bounds check
                // Drop APP1 (Exif + XMP) and COM; keep JFIF/ICC/tables/frame.
                if !matches!(marker, 0xE1 | 0xFE) {
                    out.extend_from_slice(&[0xFF, marker]);
                    out.extend_from_slice(&input[i..seg_end]);
                }
                i = seg_end;
            }
        }
    }
}

/// Drops PNG metadata chunks (`eXIf`, `tEXt`, `zTXt`, `iTXt`, `tIME`), keeping
/// the signature and every other chunk (image data plus colour management).
fn strip_png_metadata(input: &[u8]) -> Option<Vec<u8>> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if input.get(0..8)? != SIG {
        return None;
    }
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(&SIG);
    let mut i = 8;
    while i + 8 <= input.len() {
        let len = u32::from_be_bytes(input.get(i..i + 4)?.try_into().ok()?) as usize;
        let kind = input.get(i + 4..i + 8)?;
        let chunk_end = i + 12 + len; // length(4) + type(4) + data + crc(4)
        input.get(i..chunk_end)?; // bounds check
        let is_iend = kind == b"IEND";
        if !matches!(kind, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME") {
            out.extend_from_slice(&input[i..chunk_end]);
        }
        i = chunk_end;
        if is_iend {
            break;
        }
    }
    Some(out)
}

/// Drops the `EXIF` and `XMP ` chunks from a RIFF/WebP container and clears the
/// matching flag bits in `VP8X`, keeping the image/animation/alpha/ICC chunks.
fn strip_webp_metadata(input: &[u8]) -> Option<Vec<u8>> {
    if input.get(0..4)? != b"RIFF" || input.get(8..12)? != b"WEBP" {
        return None;
    }
    let mut body = Vec::with_capacity(input.len());
    let mut i = 12;
    while i + 8 <= input.len() {
        let fourcc: [u8; 4] = input.get(i..i + 4)?.try_into().ok()?;
        let size = u32::from_le_bytes(input.get(i + 4..i + 8)?.try_into().ok()?) as usize;
        let padded = size + (size & 1); // chunks are padded to an even length
        let chunk_end = i + 8 + padded;
        input.get(i..chunk_end)?; // bounds check
        match &fourcc {
            b"EXIF" | b"XMP " => {} // drop
            b"VP8X" => {
                let mut chunk = input[i..chunk_end].to_vec();
                if let Some(flags) = chunk.get_mut(8) {
                    *flags &= !0x0C; // clear EXIF (0x08) and XMP (0x04) bits
                }
                body.extend_from_slice(&chunk);
            }
            _ => body.extend_from_slice(&input[i..chunk_end]),
        }
        i = chunk_end;
    }
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(4 + body.len()).ok()?.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Some(out)
}

/// Decodes and bounds-checks an uploaded image, then produces the full
/// rendition per `full` (passthrough keeps the original format/dimensions and
/// only strips metadata; the others downscale to `max_edge` and re-encode) plus
/// — when `preview` is set — a downscaled `small` style and blurhash.
/// CPU-bound — call through the `*_blocking` wrappers from async contexts.
#[allow(
    clippy::too_many_lines,
    reason = "one linear decode → full → preview flow"
)]
fn process_image(
    input: &[u8],
    max_edge: u32,
    preview: Option<PreviewMedia>,
    full: FullMedia,
    params: &EncodeParams,
) -> Result<ProcessedImage, ApiError> {
    if input.len() > MAX_UPLOAD_BYTES.min(params.max_image_bytes) {
        return Err(unsupported("File size exceeds the limit"));
    }
    // JPEG XL decodes through jxl-oxide and is ALWAYS re-encoded — under
    // passthrough it falls to the photo branch below, so a format most
    // clients cannot display never federates as an original.
    let format = if is_jxl(input) {
        None
    } else {
        let format = image::guess_format(input)
            .map_err(|_| unsupported("File content type is not supported"))?;
        if !matches!(
            format,
            ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif
        ) {
            return Err(unsupported("File content type is not supported"));
        }
        Some(format)
    };
    // An animated image must never be re-encoded through the still pipeline:
    // decoding takes the first frame and would silently freeze the animation
    // (an APNG or animated WebP is a still format to `guess_format`). The
    // animation outranks the configured mode — pass through, strip only.
    let full = if full != FullMedia::Passthrough && is_animated_image(input) {
        FullMedia::Passthrough
    } else {
        full
    };
    let decoded = match format {
        Some(format) => image::load_from_memory_with_format(input, format)
            .map_err(|_| unsupported("File is not a readable image"))?,
        None => decode_jxl(input).ok_or_else(|| unsupported("File is not a readable image"))?,
    };
    let (width, height) = decoded.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(unsupported("Image dimensions exceed the limit"));
    }

    // Full rendition.
    let photo = Encode::Photo {
        quality: params.jpeg_quality,
    };
    let (bytes, content_type, extension, out_width, out_height) = match full {
        FullMedia::Passthrough => {
            if let Some(stripped) = format.and_then(|format| strip_metadata(input, format)) {
                // Stored as received (same format/dimensions), metadata removed.
                let (content_type, extension) = format_output(format.unwrap_or(ImageFormat::Jpeg));
                (stripped, content_type, extension, width, height)
            } else {
                // Unparseable container (or a JXL source): fall back to a
                // photo re-encode so metadata is never left in — privacy and
                // client compatibility win over exact fidelity.
                let (bytes, content_type, extension) = encode_image(&decoded, photo)?;
                (bytes, content_type, extension, width, height)
            }
        }
        FullMedia::Avif | FullMedia::Jpeg | FullMedia::Jxl => {
            let resized = if width.max(height) > max_edge {
                decoded.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
            } else {
                decoded.clone()
            };
            let encode = match full {
                FullMedia::Avif => Encode::Avif {
                    speed: params.avif_speed_full,
                    quality: params.avif_quality,
                },
                FullMedia::Jxl => Encode::Jxl {
                    ffmpeg_path: &params.ffmpeg_path,
                    distance: params.jxl_distance,
                    effort: params.jxl_effort,
                },
                _ => photo,
            };
            match encode_image(&resized, encode) {
                Ok((bytes, content_type, extension)) => {
                    let (out_width, out_height) = resized.dimensions();
                    (bytes, content_type, extension, out_width, out_height)
                }
                // JPEG XL is best-effort (the operator's ffmpeg may lack
                // libjxl): store the metadata-stripped original instead of
                // failing the upload. Other encoders never fail per-image.
                Err(error) if full == FullMedia::Jxl => {
                    tracing::warn!(%error, "JPEG XL encode failed; storing passthrough");
                    if let Some(stripped) = format.and_then(|format| strip_metadata(input, format))
                    {
                        let (content_type, extension) =
                            format_output(format.unwrap_or(ImageFormat::Jpeg));
                        (stripped, content_type, extension, width, height)
                    } else {
                        let (bytes, content_type, extension) = encode_image(&decoded, photo)?;
                        (bytes, content_type, extension, width, height)
                    }
                }
                Err(error) => return Err(error),
            }
        }
    };

    // Preview: independent of the full rendition's format, downscaled from the
    // decoded original.
    let (small, blurhash) = if let Some(preview) = preview {
        let small_image = small_style(&decoded);
        let (small_width, small_height) = small_image.dimensions();
        let small_encode = match preview {
            PreviewMedia::Avif => Encode::Avif {
                speed: params.avif_speed_preview,
                quality: params.avif_quality,
            },
            PreviewMedia::Jpeg => photo,
        };
        let (small_bytes, _, small_extension) = encode_image(&small_image, small_encode)?;
        let blurhash = blurhash_for(&small_image);
        (
            Some(SmallImage {
                bytes: small_bytes,
                width: small_width,
                height: small_height,
                extension: small_extension,
            }),
            blurhash,
        )
    } else {
        (None, None)
    };

    Ok(ProcessedImage {
        bytes,
        content_type,
        extension,
        width: out_width,
        height: out_height,
        small,
        blurhash,
    })
}

/// The `(content_type, extension)` a passthrough rendition keeps, by the
/// decoded input format (always one of the four accepted still formats).
fn format_output(format: ImageFormat) -> (&'static str, &'static str) {
    match format {
        ImageFormat::Png => ("image/png", "png"),
        ImageFormat::WebP => ("image/webp", "webp"),
        ImageFormat::Gif => ("image/gif", "gif"),
        // JPEG and anything else reaching here (the caller restricts to the four
        // still formats) keeps JPEG.
        _ => ("image/jpeg", "jpg"),
    }
}

/// Runs the avatar/header pipeline on the blocking pool: no preview/blurhash,
/// always a Mastodon-compatible photo (JPEG, or PNG for alpha) — never AVIF,
/// which Mastodon rejects for avatars and headers.
pub async fn process_image_blocking(
    input: Vec<u8>,
    max_edge: u32,
    params: EncodeParams,
) -> Result<ProcessedImage, ApiError> {
    let _permit = crate::media_gate::acquire().await;
    tokio::task::spawn_blocking(move || {
        process_image(&input, max_edge, None, FullMedia::Jpeg, &params)
    })
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?
}

/// Runs the full status-upload pipeline (original + small + blurhash) on the
/// blocking pool, honouring the operator's `full`/`preview` media settings.
pub async fn process_upload_blocking(
    input: Vec<u8>,
    full: FullMedia,
    preview: PreviewMedia,
    params: EncodeParams,
) -> Result<ProcessedImage, ApiError> {
    let _permit = crate::media_gate::acquire().await;
    tokio::task::spawn_blocking(move || {
        process_image(&input, params.max_edge, Some(preview), full, &params)
    })
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?
}

/// Re-encodes an incoming cached image (emoji/avatar/header/card) to AVIF:
/// downscaled to `max_edge`, no preview or blurhash. Returns `None` when the
/// image must be kept as arrived instead — animated (re-encoding would freeze
/// it), an unsupported/unparseable container, or over the pixel cap — so the
/// caller falls back to its passthrough path. Small slots (emoji, avatars)
/// get the slower/denser preview speed; larger ones (cards, headers) the fast
/// full-rendition speed, since they may be encoded inline on a proxy
/// cache-miss.
fn process_cached_image(
    input: &[u8],
    max_edge: u32,
    params: &EncodeParams,
) -> Option<ProcessedImage> {
    let speed = if max_edge <= AVATAR_MAX_EDGE {
        params.avif_speed_preview
    } else {
        params.avif_speed_full
    };
    if is_animated_image(input) {
        return None;
    }
    let format = image::guess_format(input).ok()?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif
    ) {
        return None;
    }
    let decoded = image::load_from_memory_with_format(input, format).ok()?;
    let (width, height) = decoded.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let resized = if width.max(height) > max_edge {
        decoded.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };
    let encode = Encode::Avif {
        speed,
        quality: params.avif_quality,
    };
    let (bytes, content_type, extension) = encode_image(&resized, encode).ok()?;
    let (out_width, out_height) = resized.dimensions();
    Some(ProcessedImage {
        bytes,
        content_type,
        extension,
        width: out_width,
        height: out_height,
        small: None,
        blurhash: None,
    })
}

/// [`process_cached_image`] on the blocking pool.
pub async fn process_cached_image_blocking(
    input: Vec<u8>,
    max_edge: u32,
    params: EncodeParams,
) -> Result<Option<ProcessedImage>, ApiError> {
    let _permit = crate::media_gate::acquire().await;
    tokio::task::spawn_blocking(move || process_cached_image(&input, max_edge, &params))
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))
}

/// The `(content_type, extension)` for storing an image as arrived, sniffed
/// from the bytes: the four decodable raster formats plus AVIF (storable
/// though not decodable). `None` for anything else.
#[must_use]
pub fn sniffed_raw_format(input: &[u8]) -> Option<(&'static str, &'static str)> {
    if is_jxl(input) {
        return Some(("image/jxl", "jxl"));
    }
    match image::guess_format(input).ok()? {
        ImageFormat::Png => Some(("image/png", "png")),
        ImageFormat::Gif => Some(("image/gif", "gif")),
        ImageFormat::Jpeg => Some(("image/jpeg", "jpg")),
        ImageFormat::WebP => Some(("image/webp", "webp")),
        ImageFormat::Avif => Some(("image/avif", "avif")),
        _ => None,
    }
}

/// Losslessly strips metadata from an as-arrived cached image when the
/// container is one we can parse, else keeps the bytes untouched (these are
/// other servers' published files — privacy-stripping is best-effort, not the
/// hard guarantee local uploads get).
#[must_use]
pub fn strip_cached_metadata(input: Vec<u8>) -> Vec<u8> {
    match image::guess_format(&input) {
        Ok(format) => strip_metadata(&input, format).unwrap_or(input),
        Err(_) => input,
    }
}

/// A resized rendition of a site upload (Mastodon's per-var styles).
pub struct SiteUploadStyle {
    pub style: String,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct ProcessedSiteUpload {
    pub original: ProcessedImage,
    pub styles: Vec<SiteUploadStyle>,
    /// Encoded from the thumbnail's `@1x` style, like Mastodon.
    pub blurhash: Option<String>,
}

/// Mastodon's `SiteUpload::STYLES[:thumbnail]` geometry (`1200x630#` and the
/// retina double).
const THUMBNAIL_STYLES: &[(&str, u32, u32)] = &[("@1x", 1200, 630), ("@2x", 2400, 1260)];
/// `SiteUpload::FAVICON_SIZES`.
const FAVICON_SIZES: &[u32] = &[16, 32, 48];
/// `SiteUpload::ANDROID_ICON_SIZES` — the set the instance `icon` entity
/// serves. (Mastodon also renders its Apple sizes; those exist for its own
/// frontend's `<link>` tags, while Plamenu's install surface uses its separate
/// bundled Apple icon.)
const APP_ICON_SIZES: &[u32] = &[36, 48, 72, 96, 144, 192, 256, 384, 512];

/// Processes an operator site upload (`thumbnail`/`mascot`/`favicon`/
/// `app_icon`): the metadata-stripped original plus the fill-cropped PNG
/// styles Mastodon derives for the slot. CPU-bound, so it runs on the
/// blocking pool.
pub async fn process_site_upload_blocking(
    var: String,
    input: Vec<u8>,
) -> Result<ProcessedSiteUpload, ApiError> {
    let _permit = crate::media_gate::acquire().await;
    tokio::task::spawn_blocking(move || process_site_upload(&var, &input))
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?
}

fn process_site_upload(var: &str, input: &[u8]) -> Result<ProcessedSiteUpload, ApiError> {
    // The original passes through the ordinary pipeline (bounds checks,
    // metadata strip); site images never need to exceed the retina thumbnail.
    // It stays JPEG/PNG (`Photo`), not AVIF: the favicon/app-icon styles below
    // must be PNG, and we re-decode the original bytes here (we cannot decode
    // AVIF).
    // Site uploads are rare operator actions; the default (historical)
    // encoder settings are deliberate — favicon/app-icon styles must stay
    // reproducible regardless of the media knobs.
    let original = process_image(input, 2400, None, FullMedia::Jpeg, &EncodeParams::default())?;
    let decoded = image::load_from_memory(&original.bytes)
        .map_err(|_| unsupported("File is not a readable image"))?;

    let geometries: Vec<(String, u32, u32)> = match var {
        "thumbnail" => THUMBNAIL_STYLES
            .iter()
            .map(|(style, w, h)| ((*style).to_owned(), *w, *h))
            .collect(),
        "favicon" => FAVICON_SIZES
            .iter()
            .map(|size| (size.to_string(), *size, *size))
            .collect(),
        "app_icon" => APP_ICON_SIZES
            .iter()
            .map(|size| (size.to_string(), *size, *size))
            .collect(),
        // Mastodon defines no styles for the mascot.
        _ => Vec::new(),
    };

    let mut styles = Vec::with_capacity(geometries.len());
    let mut blurhash = None;
    for (style, width, height) in geometries {
        let resized = decoded.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3);
        if var == "thumbnail" && style == "@1x" {
            blurhash = blurhash_for(&resized);
        }
        let mut bytes = Vec::new();
        resized
            .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| ApiError::Internal(Box::new(e)))?;
        styles.push(SiteUploadStyle {
            style,
            bytes,
            width,
            height,
        });
    }

    Ok(ProcessedSiteUpload {
        original,
        styles,
        blurhash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let img = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                width,
                height,
                image::Rgba([10, 20, 30, 128]),
            ))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                width,
                height,
                image::Rgb([10, 20, 30]),
            ))
        };
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn custom_emoji_validation_uses_configured_cap_and_names_the_remedy() {
        let image = sample_png(8, 8, false);
        assert_eq!(
            validate_emoji_image(&image, DEFAULT_MAX_EMOJI_BYTES).unwrap(),
            ("image/png", "png")
        );

        let mut oversized = image;
        oversized.resize(2 * 1024, 0);
        let error = validate_emoji_image(&oversized, 1024).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("file is 2 KiB"));
        assert!(message.contains("up to 1 KiB"));
        assert!(message.contains("ask an administrator to raise the limit"));
    }

    #[test]
    fn avif_full_option_re_encodes_and_keeps_size() {
        // The `avif` full-media option re-encodes to AVIF, opaque or not.
        let processed = process_image(
            &sample_png(100, 60, false),
            MAX_EDGE,
            None,
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(processed.content_type, "image/avif");
        assert_eq!(processed.extension, "avif");
        assert_eq!((processed.width, processed.height), (100, 60));
        assert_eq!(
            image::guess_format(&processed.bytes).unwrap(),
            ImageFormat::Avif
        );
        assert!(processed.small.is_none());
        assert!(processed.blurhash.is_none());

        // An image with transparency is AVIF too (AVIF carries alpha).
        let alpha = process_image(
            &sample_png(2400, 1200, true),
            MAX_EDGE,
            None,
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(alpha.content_type, "image/avif");
        // Large images downscale to the max edge.
        assert_eq!((alpha.width, alpha.height), (1920, 960));
    }

    #[test]
    fn jpeg_full_option_and_avatars_stay_photo() {
        // The `jpeg`/photo path (also the avatar/header path) keeps the JPEG/PNG
        // split and is never AVIF, which Mastodon rejects for avatars/headers.
        let opaque = process_image(
            &sample_png(100, 60, false),
            AVATAR_MAX_EDGE,
            None,
            FullMedia::Jpeg,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(opaque.content_type, "image/jpeg");
        let alpha = process_image(
            &sample_png(100, 60, true),
            AVATAR_MAX_EDGE,
            None,
            FullMedia::Jpeg,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(alpha.content_type, "image/png");
    }

    #[test]
    fn passthrough_keeps_format_and_dimensions() {
        // The default full-media option stores the original untouched (bar
        // metadata): a 2400px PNG stays a 2400px PNG, not downscaled or AVIF.
        let processed = process_image(
            &sample_png(2400, 1200, false),
            MAX_EDGE,
            Some(PreviewMedia::Avif),
            FullMedia::Passthrough,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(processed.content_type, "image/png");
        assert_eq!(processed.extension, "png");
        assert_eq!((processed.width, processed.height), (2400, 1200));
        assert_eq!(
            image::guess_format(&processed.bytes).unwrap(),
            ImageFormat::Png
        );
        // The preview is independent of the full format: AVIF here.
        let small = processed.small.as_ref().unwrap();
        assert_eq!(small.extension, "avif");
        assert_eq!(
            image::guess_format(&small.bytes).unwrap(),
            ImageFormat::Avif
        );
    }

    #[test]
    fn strip_jpeg_drops_app1_and_stays_decodable() {
        let jpeg = encode_image(
            &image::load_from_memory(&sample_png(8, 8, false)).unwrap(),
            Encode::Photo { quality: 85 },
        )
        .unwrap()
        .0;
        // Splice a fake APP1 (EXIF) segment right after SOI.
        let payload = b"Exif\0\0secret-gps";
        let mut with_exif = Vec::new();
        with_exif.extend_from_slice(&jpeg[0..2]); // SOI
        with_exif.extend_from_slice(&[0xFF, 0xE1]);
        with_exif.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        with_exif.extend_from_slice(payload);
        with_exif.extend_from_slice(&jpeg[2..]);
        assert!(
            with_exif.windows(4).any(|w| w == b"Exif"),
            "fixture should contain EXIF"
        );

        let stripped = strip_jpeg_metadata(&with_exif).expect("valid jpeg");
        assert!(
            !stripped.windows(4).any(|w| w == b"Exif"),
            "EXIF must be gone"
        );
        assert!(image::load_from_memory(&stripped).is_ok(), "still decodes");
    }

    #[test]
    fn strip_png_drops_text_chunks() {
        let png = sample_png(8, 8, false);
        // Insert a tEXt chunk before IEND (find the last 12 bytes = IEND chunk).
        let iend_at = png.len() - 12;
        let text_data = b"CommentHello";
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&u32::try_from(text_data.len()).unwrap().to_be_bytes());
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(text_data);
        chunk.extend_from_slice(&0u32.to_be_bytes()); // dummy CRC (stripper ignores it)
        let mut with_text = Vec::new();
        with_text.extend_from_slice(&png[..iend_at]);
        with_text.extend_from_slice(&chunk);
        with_text.extend_from_slice(&png[iend_at..]);

        let stripped = strip_png_metadata(&with_text).expect("valid png");
        assert!(
            !stripped.windows(4).any(|w| w == b"tEXt"),
            "tEXt must be gone"
        );
        assert!(image::load_from_memory(&stripped).is_ok(), "still decodes");
    }

    #[test]
    fn strip_webp_drops_exif_chunk_and_fixes_size() {
        // A minimal RIFF/WEBP with a VP8L chunk and an EXIF chunk.
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&0u32.to_le_bytes()); // size placeholder
        webp.extend_from_slice(b"WEBP");
        let mut push_chunk = |cc: &[u8; 4], data: &[u8]| {
            webp.extend_from_slice(cc);
            webp.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
            webp.extend_from_slice(data);
            if data.len() % 2 == 1 {
                webp.push(0);
            }
        };
        push_chunk(b"VP8L", b"imagedata");
        push_chunk(b"EXIF", b"secret-gps");
        let size = u32::try_from(webp.len() - 8).unwrap();
        webp[4..8].copy_from_slice(&size.to_le_bytes());

        let stripped = strip_webp_metadata(&webp).expect("valid riff");
        assert!(
            !stripped.windows(4).any(|w| w == b"EXIF"),
            "EXIF chunk must be gone"
        );
        assert!(stripped.windows(4).any(|w| w == b"VP8L"), "image data kept");
        // RIFF size header equals the trailing byte count.
        let declared = u32::from_le_bytes(stripped[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared, stripped.len() - 8);
    }

    #[test]
    fn upload_pipeline_adds_small_style_and_blurhash() {
        let processed = process_image(
            &sample_png(1920, 1080, false),
            MAX_EDGE,
            Some(PreviewMedia::Avif),
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        let small = processed.small.as_ref().unwrap();
        // Area capped at 230400 px, aspect kept: 640x360.
        assert_eq!((small.width, small.height), (640, 360));
        // The AVIF preview option encodes at the preview speed.
        assert_eq!(
            image::guess_format(&small.bytes).unwrap(),
            ImageFormat::Avif
        );
        let hash = processed.blurhash.as_ref().unwrap();
        // 4x4 components: 6 + (16 - 1) * 2 characters.
        assert_eq!(hash.len(), 36);

        // Images already under the cap keep their dimensions.
        let tiny = process_image(
            &sample_png(100, 60, false),
            MAX_EDGE,
            Some(PreviewMedia::Avif),
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        let small = tiny.small.as_ref().unwrap();
        assert_eq!((small.width, small.height), (100, 60));
        assert!(tiny.blurhash.is_some());
    }

    #[test]
    fn still_gifs_process_as_images_and_animation_is_detected() {
        // A 1x1 still GIF.
        let mut still = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([200, 0, 0])))
            .write_to(&mut std::io::Cursor::new(&mut still), ImageFormat::Gif)
            .unwrap();
        assert!(!is_animated_gif(&still));
        // A still GIF re-encodes to AVIF under the avif option like any still image.
        let processed = process_image(
            &still,
            MAX_EDGE,
            Some(PreviewMedia::Avif),
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(processed.content_type, "image/avif");

        // A two-frame GIF via the gif encoder.
        let mut animated = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut animated);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for shade in [0u8, 255u8] {
                let frame = image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([shade, 0, 0, 255]),
                ));
                encoder.encode_frame(frame).unwrap();
            }
        }
        assert!(is_animated_gif(&animated));
    }

    #[test]
    fn jxl_uploads_decode_and_always_re_encode() {
        // Encode a sample through ffmpeg's libjxl (skip when this build
        // lacks the encoder), then feed the JXL back through the pipeline:
        // it must decode via jxl-oxide and re-encode — never pass through,
        // most clients cannot display JXL.
        let source = image::load_from_memory(&sample_png(64, 40, false)).unwrap();
        let params = EncodeParams::default();
        let Ok((jxl, content_type, extension)) = encode_image(
            &source,
            Encode::Jxl {
                ffmpeg_path: &params.ffmpeg_path,
                distance: 1.0,
                effort: 4,
            },
        ) else {
            eprintln!("skipping: ffmpeg without libjxl");
            return;
        };
        assert_eq!(content_type, "image/jxl");
        assert_eq!(extension, "jxl");
        assert!(is_jxl(&jxl));
        assert!(is_still_image(&jxl));
        assert_eq!(sniffed_raw_format(&jxl), Some(("image/jxl", "jxl")));

        let processed = process_image(&jxl, MAX_EDGE, None, FullMedia::Passthrough, &params)
            .expect("jxl uploads decode");
        assert_eq!(
            processed.content_type, "image/jpeg",
            "passthrough JXL re-encodes to a photo"
        );
        assert_eq!((processed.width, processed.height), (64, 40));
    }

    #[test]
    fn animated_images_never_freeze_under_re_encoding_modes() {
        // An animated GIF under an `avif` full mode passes through instead
        // of freezing to its first frame (the M39 animated guard).
        let mut animated = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut animated);
            for shade in [0u8, 255u8] {
                let frame = image::Frame::new(image::RgbaImage::from_pixel(
                    4,
                    4,
                    image::Rgba([shade, 0, 0, 255]),
                ));
                encoder.encode_frame(frame).unwrap();
            }
        }
        let processed = process_image(
            &animated,
            MAX_EDGE,
            Some(PreviewMedia::Avif),
            FullMedia::Avif,
            &EncodeParams::default(),
        )
        .unwrap();
        assert_eq!(processed.content_type, "image/gif");
        assert_eq!(
            image::guess_format(&processed.bytes).unwrap(),
            ImageFormat::Gif
        );
        assert!(is_animated_gif(&processed.bytes), "animation survives");
        // The preview still derives from the first frame.
        assert!(processed.small.is_some());
        assert!(processed.blurhash.is_some());
    }

    #[test]
    fn gif_alpha_detection_sees_first_frame_transparency() {
        let mut opaque = Vec::new();
        let mut transparent = Vec::new();
        for (buffer, alpha) in [(&mut opaque, 255u8), (&mut transparent, 0u8)] {
            let mut encoder = image::codecs::gif::GifEncoder::new(buffer);
            for shade in [10u8, 200u8] {
                let mut image_buffer =
                    image::RgbaImage::from_pixel(4, 4, image::Rgba([shade, 0, 0, 255]));
                image_buffer.put_pixel(0, 0, image::Rgba([0, 0, 0, alpha]));
                let frame = image::Frame::new(image_buffer);
                encoder.encode_frame(frame).unwrap();
            }
        }
        assert!(!gif_first_frame_has_alpha(&opaque));
        assert!(gif_first_frame_has_alpha(&transparent));
        assert!(!gif_first_frame_has_alpha(b"not a gif"));
    }

    #[test]
    fn garbage_and_unsupported_formats_are_rejected() {
        assert!(
            process_image(
                b"not an image",
                MAX_EDGE,
                None,
                FullMedia::Avif,
                &EncodeParams::default()
            )
            .is_err()
        );
        // A BMP header: unsupported format.
        assert!(
            process_image(
                b"BM\x00\x00\x00\x00",
                MAX_EDGE,
                None,
                FullMedia::Avif,
                &EncodeParams::default()
            )
            .is_err()
        );
    }

    /// A PNG chunk: length + type + data + a placeholder CRC (the animation
    /// detector walks chunk headers, it never validates CRCs).
    fn png_chunk(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        chunk.extend_from_slice(&kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&[0; 4]);
        chunk
    }

    #[test]
    fn apng_is_detected_as_animated() {
        // `image::guess_format` sees any PNG signature as PNG; only the acTL
        // chunk (which the spec puts before IDAT) marks an APNG.
        let plain = sample_png(4, 4, false);
        assert!(!is_animated_image(&plain));

        let mut apng = Vec::new();
        apng.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        apng.extend_from_slice(&png_chunk(*b"IHDR", &[0; 13]));
        apng.extend_from_slice(&png_chunk(*b"acTL", &[0; 8]));
        apng.extend_from_slice(&png_chunk(*b"IDAT", &[0; 4]));
        apng.extend_from_slice(&png_chunk(*b"IEND", &[]));
        assert!(is_animated_image(&apng));
    }

    #[test]
    fn animated_webp_is_detected() {
        let mut still = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([0, 100, 0])))
            .write_to(&mut std::io::Cursor::new(&mut still), ImageFormat::WebP)
            .unwrap();
        assert!(!is_animated_image(&still));

        // A minimal animated container: VP8X with the animation flag set.
        let mut animated = Vec::new();
        animated.extend_from_slice(b"RIFF");
        animated.extend_from_slice(&22u32.to_le_bytes());
        animated.extend_from_slice(b"WEBP");
        animated.extend_from_slice(b"VP8X");
        animated.extend_from_slice(&10u32.to_le_bytes());
        animated.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(is_animated_image(&animated));
    }

    #[test]
    fn cached_images_recode_stills_and_keep_animations() {
        // A still image recodes to a downscaled AVIF.
        let processed = process_cached_image(
            &sample_png(600, 300, false),
            EMOJI_MAX_EDGE,
            &EncodeParams::default(),
        )
        .expect("still image must convert");
        assert_eq!(processed.content_type, "image/avif");
        assert_eq!((processed.width, processed.height), (256, 128));
        assert!(processed.small.is_none());

        // Animated input is left for the as-arrived path.
        let mut animated = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut animated);
            for shade in [0u8, 255u8] {
                let frame = image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([shade, 0, 0, 255]),
                ));
                encoder.encode_frame(frame).unwrap();
            }
        }
        assert!(
            process_cached_image(&animated, EMOJI_MAX_EDGE, &EncodeParams::default()).is_none()
        );

        // Garbage too.
        assert!(
            process_cached_image(b"not an image", EMOJI_MAX_EDGE, &EncodeParams::default())
                .is_none()
        );
    }

    #[test]
    fn sniffed_raw_format_covers_storable_containers() {
        assert_eq!(
            sniffed_raw_format(&sample_png(2, 2, false)),
            Some(("image/png", "png"))
        );
        // AVIF is storable as arrived even though it cannot be decoded.
        let avif = process_cached_image(
            &sample_png(8, 8, false),
            EMOJI_MAX_EDGE,
            &EncodeParams::default(),
        )
        .unwrap()
        .bytes;
        assert_eq!(sniffed_raw_format(&avif), Some(("image/avif", "avif")));
        assert_eq!(sniffed_raw_format(b"BM\x00\x00\x00\x00"), None);
    }
}
