//! Native rendition negotiation. All formats use the existing retained source.
use crate::{config::MusicConfig, error::ApiError};
use plamenu_db::role::{Role, permission};
use serde::{Deserialize, Serialize};
pub mod cold;
mod jobs;
pub use jobs::{
    Listener, Rendition, authorize_playback, cancel, get, get_shared, list, list_shared, negotiate,
    request, request_cold, request_shared, run_one, spawn,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Mp3,
    Ogg,
    Flac,
    Aac,
    Opus,
}
impl Format {
    pub fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "mp3" => Ok(Self::Mp3),
            "ogg" => Ok(Self::Ogg),
            "flac" => Ok(Self::Flac),
            "aac" => Ok(Self::Aac),
            "opus" => Ok(Self::Opus),
            _ => Err(ApiError::Unprocessable(
                "Choose mp3, ogg, flac, aac or opus".into(),
            )),
        }
    }
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Flac => "flac",
            Self::Aac => "aac",
            Self::Opus => "opus",
        }
    }
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Aac => "m4a",
            _ => self.name(),
        }
    }
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Ogg => "audio/ogg",
            Self::Flac => "audio/flac",
            Self::Aac => "audio/mp4",
            Self::Opus => "audio/opus",
        }
    }
    #[must_use]
    pub fn codec(self) -> &'static str {
        match self {
            Self::Mp3 => "libmp3lame",
            Self::Ogg => "libvorbis",
            Self::Flac => "flac",
            Self::Aac => "aac",
            Self::Opus => "libopus",
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Selection {
    pub format: Format,
    pub bitrate: u32,
}
/// libvorbis's nominal-rate setup tables are per channel and sample-rate
/// family. Preserve the source layout; never upmix mono to satisfy a bitrate.
/// <https://github.com/xiph/vorbis/tree/master/lib/modes>
fn vorbis_rate(bitrate: u32, sample_rate: i32, channels: i32) -> Result<u32, ApiError> {
    if !(1..=255).contains(&channels) {
        return Err(ApiError::Unprocessable(
            "Unsupported audio channel count".into(),
        ));
    }
    // At shared endpoints take the union of the adjacent setup families,
    // matching libvorbis's ordered search for a usable nominal-rate template.
    let families = [
        (8000, 9000, 8, 42, 6, 32),
        (9000, 15000, 12, 50, 8, 44),
        (15000, 19000, 16, 100, 12, 86),
        (19000, 26000, 16, 90, 15, 86),
        (26000, 40000, 30, 190, 18, 190),
        (40000, 50000, 32, 240, 23, 250),
    ];
    let count = u32::try_from(channels).expect("validated positive channel count");
    let mut selected = None;
    for (low, high, mono_min, mono_max, stereo_min, stereo_max) in families {
        if (low..=high).contains(&sample_rate) {
            let (minimum, maximum) = if channels == 2 {
                (stereo_min, stereo_max)
            } else {
                (mono_min, mono_max)
            };
            let candidate = bitrate.min(maximum * count);
            if candidate >= minimum * count {
                selected =
                    Some(selected.map_or(candidate, |previous: u32| previous.max(candidate)));
            }
        }
    }
    // Surround has its own setup at 40–70 kHz, including below the
    // uncoupled minimum. Other >50 kHz modes support quality, not bitrate.
    if channels == 6 && (40000..=70000).contains(&sample_rate) && bitrate >= 84 {
        selected = Some(selected.map_or(bitrate, |previous| previous.max(bitrate)));
    }
    selected.ok_or_else(|| ApiError::Unprocessable(
        "Vorbis cannot preserve this sample rate and channel layout at the allowed bitrate; choose another format".into(),
    ))
}
/// Adapt role ceilings to Plamenu permission roles; renaming a role never
/// elevates it. Anonymous readers use the ordinary member ceiling.
pub fn selection(
    config: MusicConfig,
    format: &str,
    requested: Option<u32>,
    role: Option<&Role>,
) -> Result<Selection, ApiError> {
    let format = Format::parse(format)?;
    if requested.is_some_and(|value| !(32..=512).contains(&value)) {
        return Err(ApiError::Unprocessable(
            "Bitrate must be 32 to 512 kbps".into(),
        ));
    }
    let role_max = if role.is_some_and(|role| role.can(permission::MANAGE_SETTINGS)) {
        config.admin_max_bitrate
    } else if role.is_some_and(|role| role.can(permission::MANAGE_REPORTS)) {
        config.moderator_max_bitrate
    } else {
        config.user_max_bitrate
    };
    let mut bitrate = requested
        .unwrap_or(config.default_bitrate)
        .min(config.max_bitrate)
        .min(role_max);
    // LAME chooses a standard CBR rate. Round down so a custom instance/role
    // ceiling such as 250 kbps cannot produce a 256 kbps MP3.
    if format == Format::Mp3 {
        bitrate = [
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ]
        .into_iter()
        .rev()
        .find(|value| *value <= bitrate)
        .unwrap_or(32);
    }
    Ok(Selection { format, bitrate })
}

/// Output and scratch stay owned together until the admitted caller persists
/// the result. Dropping a cancelled encode removes its temporary artifacts.
pub struct Encoded {
    pub directory: tempfile::TempDir,
    pub file: std::path::PathBuf,
    pub bytes: u64,
    pub selection: Selection,
}
/// Encode one already-authorized local source. The orchestration caller owns
/// the job/scratch reservation and current-source fence; this helper cannot
/// fetch URLs, admit jobs, or publish a rendition by itself.
pub async fn encode(
    config: &crate::config::Config,
    source: &std::path::Path,
    selection: Selection,
    limit: &crate::media_limits::FileSizeLimit,
) -> Result<Encoded, ApiError> {
    let selection = for_source(config, source, selection).await?;
    let directory = crate::media_transcode::scratch_dir(config).await?;
    encode_in(config, source, selection, limit, directory).await
}
pub(crate) async fn for_source(
    config: &crate::config::Config,
    source: &std::path::Path,
    mut selection: Selection,
) -> Result<Selection, ApiError> {
    if selection.format == Format::Ogg {
        let source_probe = crate::media_transcode::probe_music(config, source).await?;
        selection.bitrate = vorbis_rate(
            selection.bitrate,
            source_probe.audio_sample_rate.unwrap_or(0),
            source_probe.audio_channels.unwrap_or(0),
        )?;
    }
    Ok(selection)
}
pub(crate) async fn encode_in(
    config: &crate::config::Config,
    source: &std::path::Path,
    selection: Selection,
    limit: &crate::media_limits::FileSizeLimit,
    directory: tempfile::TempDir,
) -> Result<Encoded, ApiError> {
    if limit.bytes == 0 || limit.bytes > config.music.max_rendition_mib.saturating_mul(1024 * 1024)
    {
        return Err(ApiError::Unprocessable(
            "Invalid rendition file budget".into(),
        ));
    }
    let file = directory
        .path()
        .join(format!("encoded.{}", selection.format.extension()));
    let mut args: Vec<std::ffi::OsString> = ["-y", "-i"].into_iter().map(Into::into).collect();
    args.push(source.as_os_str().to_owned());
    args.extend(
        [
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-c:a",
            selection.format.codec(),
        ]
        .into_iter()
        .map(Into::into),
    );
    if selection.format != Format::Flac {
        args.push("-b:a".into());
        args.push(format!("{}k", selection.bitrate).into());
    }
    if selection.format == Format::Aac {
        // Fragmented MP4 starts with its initialization metadata. A regular
        // fast-start M4A rewrites that metadata only when encoding finishes.
        args.extend(
            [
                "-movflags",
                "+empty_moov+default_base_moof",
                "-frag_duration",
                "1000000",
            ]
            .into_iter()
            .map(Into::into),
        );
    }
    args.extend(["-flush_packets", "1"].into_iter().map(Into::into));
    args.push(file.as_os_str().to_owned());
    let _permit = crate::media_gate::acquire().await;
    crate::media_transcode::run_ffmpeg(
        config,
        args,
        std::time::Duration::from_mins(30),
        Some(limit),
    )
    .await?;
    let bytes = tokio::fs::metadata(&file)
        .await
        .map_err(|error| ApiError::Internal(Box::new(error)))?
        .len();
    if bytes == 0 || bytes > limit.bytes {
        return Err(ApiError::Unprocessable(
            "Rendition exceeded its file budget".into(),
        ));
    }
    // Probe the completed container rather than trusting its extension or a
    // successful ffmpeg exit. The returned artifact is still private scratch.
    let probe = crate::media_transcode::probe_music(config, &file).await?;
    if probe.audio_channels.is_none()
        || probe
            .duration
            .is_none_or(|duration| !duration.is_finite() || duration <= 0.0)
    {
        return Err(ApiError::Unprocessable(
            "Rendition has no playable audio".into(),
        ));
    }
    Ok(Encoded {
        directory,
        file,
        bytes,
        selection,
    })
}
