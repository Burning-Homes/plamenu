//! A persistent encoder accepts bounded PCM from separately authorized decoders.
use super::Output;
use crate::{AppState, error::ApiError, music_renditions::Format};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
};

pub struct Encoder {
    pub child: Child,
    pub input: ChildStdin,
    pub output: ChildStdout,
}
fn internal(error: std::io::Error) -> ApiError {
    ApiError::Internal(Box::new(error))
}
pub fn encoder(state: &AppState, row: &Output, launcher: &Path) -> Result<Encoder, ApiError> {
    let format = Format::parse(&row.format)?;
    let limit = crate::media_limits::FileSizeLimit {
        launcher: launcher.to_owned(),
        bytes: 8 * 1024 * 1024,
    };
    let mut command =
        crate::media_limits::command(Path::new(&state.config.ffmpeg_path), Some(&limit));
    let muxer = if format == Format::Mp3 { "mp3" } else { "ogg" };
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-filter_threads",
            "1",
            "-threads",
            "1",
            "-f",
            "s16le",
            "-ar",
            &row.sample_rate.to_string(),
            "-ac",
            "2",
            "-i",
            "pipe:0",
            "-c:a",
            format.codec(),
            "-b:a",
            &format!("{}k", row.bitrate),
            "-flush_packets",
            "1",
            "-f",
            muxer,
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(internal)?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| internal(std::io::Error::other("Missing encoder input")))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| internal(std::io::Error::other("Missing encoder output")))?;
    Ok(Encoder {
        child,
        input,
        output,
    })
}
pub async fn pump(state: AppState, row: Output, mut output: ChildStdout) -> Result<(), ApiError> {
    let mut sequence = 0_i64;
    let mut header = super::ogg::Header::default();
    let mut buffer = vec![0; 16384];
    let mut beat = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        tokio::select! {
            ()=state.shutdown.cancelled()=>return Ok(()),
            _=beat.tick()=>{
                if !super::worker::heartbeat(&state,&row).await? {return Ok(());}
            },
            read=output.read(&mut buffer)=>{
                let count=read.map_err(internal)?;
                if count==0 {return Ok(());}
                if row.format!="mp3" && let Some(header)=header.push(&buffer[..count],row.format=="opus")? {
                    sqlx::query("UPDATE music_outputs SET codec_header=$4 WHERE id=$1 AND generation=$2 AND worker_lease=$3 AND lease_until>now()")
                        .bind(row.id).bind(row.generation).bind(row.worker_lease).bind(header).execute(&state.pool).await.map_err(plamenu_db::DbError::from)?;
                }
                sequence=sequence.checked_add(1).ok_or_else(||internal(std::io::Error::other("Audio sequence exhausted")))?;
                super::worker::publish(&state,&row,sequence,&buffer[..count]).await?;
            },
        }
    }
}

fn decoder(
    state: &AppState,
    row: &Output,
    source: &super::source::Input,
    launcher: &Path,
    position: f64,
) -> Result<Child, ApiError> {
    let limit = crate::media_limits::FileSizeLimit {
        launcher: launcher.to_owned(),
        bytes: 8 * 1024 * 1024,
    };
    let mut command =
        crate::media_limits::command(Path::new(&state.config.ffmpeg_path), Some(&limit));
    let local = matches!(
        source,
        super::source::Input::File(_) | super::source::Input::Retained(_)
    );
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-threads",
        "1",
        "-re",
        "-protocol_whitelist",
        if local { "file" } else { "pipe" },
        "-format_whitelist",
        &crate::media_transcode::ACCEPTED_FORMATS.join(","),
    ]);
    if local {
        command.args(["-ss", &position.to_string()]);
    }
    command.arg("-i");
    match source {
        super::source::Input::File(original) => {
            command.arg(&original.path);
        }
        super::source::Input::Retained(path) => {
            command.arg(path);
        }
        super::source::Input::Origin(_) => {
            command.arg("pipe:0").args(["-ss", &position.to_string()]);
        }
    }
    command
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-dn",
            "-f",
            "s16le",
            "-ar",
            &row.sample_rate.to_string(),
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdin(if local { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.spawn().map_err(internal)
}

/// Return false on a newer queue command so the same encoder can switch sources.
pub async fn track(
    state: &AppState,
    row: &Output,
    entry: &crate::music_collections::Entry,
    input: &mut ChildStdin,
    launcher: &Path,
    queue: &crate::music_queues::Queue,
    listen: &mut super::playback::Listen,
) -> Result<bool, ApiError> {
    let source = super::source::resolve(state, entry, row.owner_id).await?;
    let mut child = decoder(state, row, &source, launcher, queue.position_seconds)?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| internal(std::io::Error::other("Missing decoder output")))?;
    let mut origin = match &source {
        super::source::Input::File(_) | super::source::Input::Retained(_) => None,
        super::source::Input::Origin(url) => Some(Box::pin(super::source::copy_origin(
            state,
            url,
            child
                .stdin
                .take()
                .ok_or_else(|| internal(std::io::Error::other("Missing decoder input")))?,
        ))),
    };
    let mut buffer = vec![0; 16384];
    let mut beat = tokio::time::interval(std::time::Duration::from_secs(1));
    listen.new_segment();
    let mut position = queue.position_seconds;
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            ()=state.shutdown.cancelled()=>return Ok(false),
            _=beat.tick()=>{
                if started.elapsed()>std::time::Duration::from_hours(6) {return Err(ApiError::ServiceUnavailable("HTTP audio source reached its time limit".into()));}
                if !super::worker::heartbeat(state,row).await? || !super::playback::checkpoint(state,row,queue,position).await? {
                    listen.report(state,row,position-queue.position_seconds,false).await?;
                    return Ok(false);
                }
                listen.report(state,row,position-queue.position_seconds,false).await?;
                if super::source::resolve(state,entry,row.owner_id).await?!=source {return Err(ApiError::NotFound);}
            },
            result=async {origin.as_mut().expect("origin branch is guarded").await},if origin.is_some()=>{
                result?;
                origin=None;
            },
            read=output.read(&mut buffer)=>{
                let count=read.map_err(internal)?;
                if count==0 {break;}
                tokio::time::timeout(std::time::Duration::from_secs(10),input.write_all(&buffer[..count])).await
                    .map_err(|_|ApiError::ServiceUnavailable("HTTP audio encoder stalled".into()))?.map_err(internal)?;
                position+=f64::from(u32::try_from(count).expect("bounded PCM buffer"))/(f64::from(row.sample_rate)*4.0);
            },
        }
    }
    if !child.wait().await.map_err(internal)?.success() {
        return Err(ApiError::BadGateway("HTTP audio decoder failed".into()));
    }
    let complete = super::playback::checkpoint(state, row, queue, position).await?;
    listen
        .report(state, row, position - queue.position_seconds, complete)
        .await?;
    Ok(complete)
}
