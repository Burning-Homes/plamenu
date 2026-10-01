//! One bounded encoder, independently failing muxer outputs. No filesystem
//! pathname is interpolated into `FFmpeg`'s tee mini-language.
use super::InputFormat;
use crate::{config::Config, media_limits::FileSizeLimit, storage::LiveReplicaWriter};
use std::{io, path::Path};

pub(super) fn encoder(
    config: &Config,
    output: &Path,
    format: InputFormat,
    segment_seconds: i32,
    list_size: i32,
    replica: Option<&std::fs::File>,
) -> io::Result<tokio::process::Command> {
    let limit = FileSizeLimit {
        launcher: crate::media_limits::running_executable()?,
        bytes: config.native_video.max_upload_bytes(),
    };
    let mut command = crate::media_limits::command(Path::new(&config.ffmpeg_path), Some(&limit));
    command.current_dir(output);
    command.args(["-hide_banner", "-loglevel", "error", "-fflags", "+genpts"]);
    match format {
        InputFormat::Flv | InputFormat::HttpFlv => {
            command.args([
                "-f", "flv", "-i", "pipe:0", "-map", "0:v:0?", "-map", "0:a:0?", "-c", "copy",
            ]);
        }
        InputFormat::BrowserWebm => {
            let gop = (segment_seconds * 30).to_string();
            let threads = std::thread::available_parallelism()
                .map_or(1, usize::from)
                .checked_div(config.native_video.jobs * 2)
                .unwrap_or(1)
                .clamp(1, 8)
                .to_string();
            command.args(["-filter_threads", "1", "-threads", &threads]);
            // MediaRecorder supplies codec parameters in its Matroska header.
            // Keep a finite one-second analysis window, not FFmpeg's generic
            // five-second probe queued ahead of live wall-clock processing.
            command.args(["-re", "-analyzeduration", "1000000", "-probesize", "262144", "-f", "matroska", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0?",
                "-vf", "scale=w='min(1280,iw)':h='min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2",
                "-r", "30", "-fps_mode", "cfr", "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                "-pix_fmt", "yuv420p", "-g", &gop, "-b:v", "2500k", "-maxrate", "2500k",
                "-bufsize", "5000k", "-c:a", "aac", "-b:a", "128k"]);
            command.args(["-threads", &threads]);
        }
        InputFormat::BrowserMp4 => {
            command.args([
                "-f", "mp4", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0?", "-c", "copy",
            ]);
        }
    }
    let failure = if replica.is_some() {
        ":onfail=ignore"
    } else {
        ""
    };
    // Cover both committed windows and a bounded checkpoint/drain interval.
    // FFmpeg must not delete a segment still named by the durable snapshot.
    let deletion = if replica.is_some() {
        (list_size * 2).max(30 / segment_seconds + 3)
    } else {
        1
    };
    let slave = |prefix: &str| {
        format!(
            "[f=hls{failure}:hls_time={segment_seconds}:hls_list_size={list_size}:hls_delete_threshold={deletion}:hls_flags=delete_segments+omit_endlist+independent_segments+temp_file:hls_segment_filename={prefix}segment%06d.ts]{prefix}index.m3u8|[f=mp4{failure}:movflags=+frag_keyframe+empty_moov+default_base_moof]{prefix}replay.mp4"
        )
    };
    let mut tee = slave("");
    #[cfg(target_os = "linux")]
    if let Some(directory) = replica {
        use std::os::fd::AsRawFd;
        // Parent-owned descriptor, not /proc/self/fd: no unsafe pre-exec hook
        // or CLOEXEC race. Same-UID children resolve this immutable directory
        // handle; kernel parent-death protection stops them when it disappears.
        let alias = format!("/proc/{}/fd/{}/", std::process::id(), directory.as_raw_fd());
        tee.push('|');
        tee.push_str(&slave(&alias));
    }
    command.args(["-flags", "+global_header", "-f", "tee", &tee]);
    Ok(command)
}

pub(super) async fn resource_state(
    config: &Config,
    primary: &Path,
    replica: Option<&LiveReplicaWriter>,
) -> (bool, bool) {
    let floor =
        config.native_video.min_free_gib * crate::config::NativeVideoConfig::GIB + 8 * 1024 * 1024;
    let cap = config.native_video.max_upload_bytes();
    let ceiling = cap.saturating_sub((cap / 4).min(8 * 1024 * 1024));
    let mut healthy = false;
    let mut capped = false;
    for (directory, required) in std::iter::once((primary, floor))
        .chain(replica.map(|target| (target.directory.as_path(), floor.max(target.min_free_bytes))))
    {
        if replica.is_some_and(|target| target.directory == directory && !same_directory(target)) {
            continue;
        }
        healthy |=
            matches!(crate::storage::available_bytes(directory), Ok(free) if free >= required);
        capped |= tokio::fs::symlink_metadata(directory.join("replay.mp4"))
            .await
            .is_ok_and(|metadata| {
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() >= ceiling
            });
    }
    (!healthy, capped)
}

fn same_directory(target: &LiveReplicaWriter) -> bool {
    let Ok(current) = std::fs::symlink_metadata(&target.directory) else {
        return false;
    };
    let Ok(opened) = target.directory_handle.metadata() else {
        return false;
    };
    if !current.is_dir() || current.file_type().is_symlink() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        current.dev() == opened.dev() && current.ino() == opened.ino()
    }
    #[cfg(not(unix))]
    {
        false
    }
}
