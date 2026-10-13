//! Source tag writeback remuxes codecs and verifies unchanged decoded audio.
use super::fs::{Opened, Root, Staged};
use crate::{AppState, error::ApiError, music_assets::Asset};
use std::process::Stdio;
pub(super) async fn prepare(
    state: &AppState,
    root: &Root,
    path: &str,
    original: &Opened,
    asset: &Asset,
    launcher: &std::path::Path,
) -> Result<Staged, ApiError> {
    if original.fingerprint.bytes > state.config.music.max_download_item_mib * 1024 * 1024 {
        return Err(ApiError::Unprocessable(
            "Source exceeds the tag writeback size limit".into(),
        ));
    }
    let replacement = root
        .stage(path)
        .map_err(|_| ApiError::Conflict("Cannot prepare source tag writeback".into()))?;
    // Equal PCM may be referenced in different containers. Write the selected
    // physical file's format, rather than the catalogue's primary original.
    let probe =
        crate::media_transcode::probe_music(&state.config, &original.process_path()).await?;
    let current = crate::music_assets::describe_import(probe, &asset.title)?;
    let format = current.format;
    let mux = match format {
        "m4a" => "mp4",
        "mka" => "matroska",
        "wma" => "asf",
        "aac" => "adts",
        other => other,
    };
    let cover = super::cover::load(state, asset).await?;
    let permit = crate::media_gate::acquire().await;
    let limit = crate::media_limits::FileSizeLimit {
        launcher: launcher.to_owned(),
        bytes: state.config.music.max_download_item_mib * 1024 * 1024,
    };
    let mut command = crate::media_limits::command(
        std::path::Path::new(&state.config.ffmpeg_path),
        Some(&limit),
    );
    command
        .args(["-nostdin", "-v", "error", "-y"])
        .args(crate::media_transcode::local_input_options())
        .arg("-i")
        .arg(original.process_path());
    map_cover(&mut command, cover.as_ref(), format);
    set_tags(&mut command, asset);
    let mut child = command
        .args(["-f", mux])
        .arg(replacement.process_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let result = tokio::select! {
        result=tokio::time::timeout(std::time::Duration::from_mins(5),child.wait())=>result.map_err(|_|ApiError::Unprocessable("Tag writeback timed out".into()))?.map_err(|error|ApiError::Internal(Box::new(error)))?,
        ()=state.shutdown.cancelled()=>return Err(ApiError::Conflict("Source tag writeback stopped".into())),
    };
    if !result.success() {
        return Err(ApiError::Unprocessable(
            "This container does not support source tag writeback".into(),
        ));
    }
    drop(permit);
    let probe =
        crate::media_transcode::probe_music(&state.config, &replacement.process_path()).await?;
    let description = crate::music_assets::describe_import(probe, &asset.title)?;
    let hash = crate::music_assets::fingerprint(
        state,
        &replacement.process_path(),
        &description.properties,
    )
    .await?;
    if hash != asset.audio_hash {
        return Err(ApiError::Conflict(
            "Tag writeback changed audio; original retained".into(),
        ));
    }
    Ok(replacement)
}

fn map_cover(
    command: &mut tokio::process::Command,
    cover: Option<&super::cover::Cover>,
    format: &str,
) {
    if let Some(cover) = cover {
        if matches!(format, "ogg" | "opus") {
            command
                .args([
                    "-protocol_whitelist",
                    "file",
                    "-format_whitelist",
                    "ffmetadata",
                    "-f",
                    "ffmetadata",
                    "-i",
                ])
                .arg(cover.metadata.path());
            command.args([
                "-map",
                "0:a",
                "-c",
                "copy",
                "-map_metadata",
                "1",
                "-map_metadata",
                "0",
            ]);
        } else if matches!(format, "mp3" | "m4a" | "flac") {
            command
                .args([
                    "-protocol_whitelist",
                    "file",
                    "-format_whitelist",
                    "image2",
                    "-f",
                    "image2",
                    "-i",
                ])
                .arg(cover.png.path());
            command.args([
                "-map",
                "0:a",
                "-map",
                "1:v:0",
                "-c",
                "copy",
                "-disposition:v",
                "attached_pic",
                "-map_metadata",
                "0",
            ]);
        } else {
            command.args(["-map", "0", "-c", "copy", "-map_metadata", "0"]);
        }
    } else {
        command.args(["-map", "0", "-c", "copy", "-map_metadata", "0"]);
    }
}

fn set_tags(command: &mut tokio::process::Command, asset: &Asset) {
    for (key, value) in [
        ("title", asset.title.clone()),
        ("artist", asset.artist.clone()),
        ("album", asset.album.clone()),
        ("genre", asset.genre.clone()),
        (
            "track",
            asset
                .track_number
                .map(|v| v.to_string())
                .unwrap_or_default(),
        ),
        (
            "disc",
            asset.disc_number.map(|v| v.to_string()).unwrap_or_default(),
        ),
        (
            "date",
            asset
                .release_year
                .map(|v| v.to_string())
                .unwrap_or_default(),
        ),
        (
            "musicbrainz_trackid",
            asset.musicbrainz_id.clone().unwrap_or_default(),
        ),
    ] {
        command.arg("-metadata").arg(format!("{key}={value}"));
    }
}
