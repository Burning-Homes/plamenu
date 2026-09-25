//! Native RTMP ingest. A publish key authorizes exactly one fileless Video;
//! media is remuxed once into a bounded sliding HLS window, never per viewer.
//! Every session tees the same encoded packets into a replay MP4.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr};
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use plamenu_db::native_live::FinishReason;
use rtmp_rs::media::flv::{FlvTag, FlvTagType};
use rtmp_rs::protocol::message::{ConnectParams, PlayParams, PublishParams};
use rtmp_rs::registry::StreamRegistry;
use rtmp_rs::server::connection::Connection;
use rtmp_rs::server::handler::MediaDeliveryMode;
use rtmp_rs::session::{SessionContext, StreamContext};
use rtmp_rs::{AuthResult, RtmpHandler, ServerConfig};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

use crate::config::{Config, LiveRtmpsConfig};
use crate::state::AppState;

const MAX_TAG_BYTES: usize = 2 * 1024 * 1024;
const QUEUE_TAGS: usize = 32;
const BROWSER_QUEUE_CHUNKS: usize = 8;

#[derive(Clone)]
pub struct PublisherInput {
    pub sender: mpsc::Sender<Vec<u8>>,
    stopped: Arc<AtomicBool>,
    revoked: Arc<AtomicBool>,
}

impl PublisherInput {
    pub fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
}

struct PumpControl {
    stopped: Arc<AtomicBool>,
    revoked: Arc<AtomicBool>,
    disconnect: Option<Arc<std::net::TcpStream>>,
}

#[derive(Clone, Copy)]
pub enum InputFormat {
    Flv,
    HttpFlv,
    BrowserWebm,
    BrowserMp4,
}

/// Credentials stay out of publisher URLs and are compared by digest in the
/// database. RTMP and HTTPS-FLV deliberately share the same key format.
pub(crate) fn key_digest(key: &str) -> Option<[u8; 32]> {
    if key.len() != 43
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return None;
    }
    Some(Sha256::digest(key.as_bytes()).into())
}

#[derive(Clone)]
struct IngestHandler {
    state: AppState,
    publishers: Arc<Mutex<HashMap<u64, PublisherInput>>>,
    disconnects: Arc<Mutex<HashMap<u64, Arc<std::net::TcpStream>>>>,
}

impl IngestHandler {
    async fn close(&self, session_id: u64, stopped: bool) {
        if let Some(publisher) = self.publishers.lock().await.remove(&session_id)
            && stopped
        {
            publisher.mark_stopped();
        }
    }
}

impl RtmpHandler for IngestHandler {
    fn on_connect(
        &self,
        _ctx: &SessionContext,
        params: &ConnectParams,
    ) -> impl std::future::Future<Output = AuthResult> + Send {
        std::future::ready(if params.app == "live" {
            AuthResult::Accept
        } else {
            AuthResult::Reject("Unknown application".into())
        })
    }

    async fn on_publish(&self, ctx: &SessionContext, params: &PublishParams) -> AuthResult {
        if self.publishers.lock().await.contains_key(&ctx.session_id) {
            return AuthResult::Reject("One broadcast per connection".into());
        }
        let key = params.stream_key.split('?').next().unwrap_or_default();
        let Some(hash) = key_digest(key) else {
            return AuthResult::Reject("Invalid stream key".into());
        };
        let session = match plamenu_db::native_live::claim(&self.state.pool, &hash).await {
            Ok(Some(session)) => session,
            Ok(None) => return AuthResult::Reject("Broadcast unavailable".into()),
            Err(error) => {
                tracing::error!(%error, "live publisher claim failed");
                return AuthResult::Reject("Publisher unavailable".into());
            }
        };
        let output = session_dir(&self.state, &session);
        if let Err(error) = tokio::fs::create_dir_all(&output).await {
            tracing::error!(%error, "live output directory failed");
            let _ = plamenu_db::native_live::finish(
                &self.state.pool,
                &session,
                FinishReason::Interrupted,
            )
            .await;
            return AuthResult::Reject("Media storage unavailable".into());
        }
        let (tx, rx) = mpsc::channel(QUEUE_TAGS);
        let stopped = Arc::new(AtomicBool::new(false));
        let revoked = Arc::new(AtomicBool::new(false));
        self.publishers.lock().await.insert(
            ctx.session_id,
            PublisherInput {
                sender: tx,
                stopped: stopped.clone(),
                revoked: revoked.clone(),
            },
        );
        let state = self.state.clone();
        let live_session = session.clone();
        let disconnect = self.disconnects.lock().await.get(&ctx.session_id).cloned();
        tokio::spawn(async move {
            pump(
                state,
                live_session,
                output,
                rx,
                InputFormat::Flv,
                PumpControl {
                    stopped,
                    revoked,
                    disconnect,
                },
            )
            .await;
        });
        if let Err(error) = crate::native_live::federate_state(&self.state, session.status_id).await
        {
            tracing::warn!(%error, media = session.media_id, "could not announce live start");
        }
        AuthResult::Accept
    }

    fn on_play(
        &self,
        _ctx: &SessionContext,
        _params: &PlayParams,
    ) -> impl std::future::Future<Output = AuthResult> + Send {
        std::future::ready(AuthResult::Reject("Use the HTTPS playback endpoint".into()))
    }

    async fn on_media_tag(&self, ctx: &StreamContext, tag: &FlvTag) -> bool {
        if tag.data.len() > MAX_TAG_BYTES {
            return false;
        }
        let Some(sender) = self
            .publishers
            .lock()
            .await
            .get(&ctx.session.session_id)
            .cloned()
        else {
            return false;
        };
        if sender.is_revoked() {
            return false;
        }
        let Some(bytes) = serialize_tag(tag) else {
            return false;
        };
        sender.sender.send(bytes).await.is_ok()
    }

    async fn on_unpublish(&self, ctx: &StreamContext) {
        self.close(ctx.session.session_id, true).await;
    }

    async fn on_disconnect(&self, ctx: &SessionContext) {
        self.close(ctx.session_id, false).await;
    }

    fn media_delivery_mode(&self) -> MediaDeliveryMode {
        MediaDeliveryMode::RawFlv
    }
}

fn serialize_tag(tag: &FlvTag) -> Option<Vec<u8>> {
    let tag_type = match tag.tag_type {
        FlvTagType::Audio => 8,
        FlvTagType::Video => 9,
        FlvTagType::Script => 18,
    };
    let len = u32::try_from(tag.data.len()).ok()?;
    if len > 0x00ff_ffff {
        return None;
    }
    let mut out = Vec::with_capacity(15 + tag.data.len());
    out.push(tag_type);
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.extend_from_slice(&tag.timestamp.to_be_bytes()[1..]);
    out.push((tag.timestamp >> 24) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&tag.data);
    out.extend_from_slice(&(len + 11).to_be_bytes());
    Some(out)
}

fn session_dir(state: &AppState, session: &plamenu_db::native_live::ActiveSession) -> PathBuf {
    state
        .config
        .media_dir
        .join("native-live")
        .join(session.media_id.to_string())
        .join(&session.session_id)
}

/// Ignore historical unpublished rows without local scratch, and do not let
/// malformed persisted identities turn a recovery scan into path traversal.
pub async fn has_crashed_recording_dir(media_dir: &Path, media_id: i64, session_id: &str) -> bool {
    if !crate::routes::native_live_hls::valid_uuid(session_id) {
        return false;
    }
    tokio::fs::symlink_metadata(
        media_dir
            .join("native-live")
            .join(media_id.to_string())
            .join(session_id),
    )
    .await
    .is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Piped browser/HTTPS chunks and RTMP tags share the same HLS window/session
/// lifecycle. The publisher is authenticated before this function is called.
pub async fn start_piped(
    state: &AppState,
    session: plamenu_db::native_live::ActiveSession,
    format: InputFormat,
) -> io::Result<PublisherInput> {
    let output = session_dir(state, &session);
    tokio::fs::create_dir_all(&output).await?;
    let (sender, receiver) = mpsc::channel(BROWSER_QUEUE_CHUNKS);
    let stopped = Arc::new(AtomicBool::new(false));
    let revoked = Arc::new(AtomicBool::new(false));
    tokio::spawn(pump(
        state.clone(),
        session,
        output,
        receiver,
        format,
        PumpControl {
            stopped: stopped.clone(),
            revoked: revoked.clone(),
            disconnect: None,
        },
    ));
    Ok(PublisherInput {
        sender,
        stopped,
        revoked,
    })
}

/// Startup follows database session recovery and precedes the listener. Keep
/// only unpublished recordings which the recovery worker can
/// still finalize. Unexpected files and symlinks are left for inspection.
pub async fn scavenge_crash_scratch<S: std::hash::BuildHasher>(
    media_dir: &Path,
    recoverable: &HashSet<(i64, String), S>,
) -> io::Result<usize> {
    let root = media_dir.join("native-live");
    let root_meta = match tokio::fs::symlink_metadata(&root).await {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !root_meta.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native-live scratch root is not a directory",
        ));
    }
    let mut removed = 0;
    let mut media = tokio::fs::read_dir(&root).await?;
    while let Some(media_entry) = media.next_entry().await? {
        let Ok(media_id) = media_entry.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        if !media_entry.file_type().await?.is_dir() {
            continue;
        }
        let mut sessions = tokio::fs::read_dir(media_entry.path()).await?;
        while let Some(session_entry) = sessions.next_entry().await? {
            let name = session_entry.file_name();
            let session_id = name.to_string_lossy();
            if !session_entry.file_type().await?.is_dir()
                || !crate::routes::native_live_hls::valid_uuid(&session_id)
                || recoverable.contains(&(media_id, session_id.into_owned()))
            {
                continue;
            }
            tokio::fs::remove_dir_all(session_entry.path()).await?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[allow(
    clippy::too_many_lines,
    reason = "one publisher's ingest, stop and replay lifecycle"
)]
async fn pump(
    state: AppState,
    session: plamenu_db::native_live::ActiveSession,
    output: PathBuf,
    mut rx: mpsc::Receiver<Vec<u8>>,
    format: InputFormat,
    control: PumpControl,
) {
    let replay = output.join("replay.mp4");
    // PeerTube-compatible latency modes. The list is a bounded sliding DVR
    // window, not an unbounded recording; replay.mp4 remains the archive.
    let segment_seconds = match session.playback.latency_mode {
        3 => 1,
        2 => 4,
        _ => 2,
    };
    let list_size = if session.playback.dvr_window_seconds == 0 {
        6
    } else {
        session.playback.dvr_window_seconds / segment_seconds
    };
    let tee_output = format!(
        "[f=hls:hls_time={segment_seconds}:hls_list_size={list_size}:hls_flags=delete_segments+omit_endlist+independent_segments+temp_file:hls_segment_filename=segment%06d.ts]index.m3u8|[f=mp4:movflags=+frag_keyframe+empty_moov+default_base_moof]replay.mp4"
    );
    let mut command = tokio::process::Command::new(&state.config.ffmpeg_path);
    command.current_dir(&output);
    command.args(["-hide_banner", "-loglevel", "error", "-fflags", "+genpts"]);
    match format {
        InputFormat::Flv | InputFormat::HttpFlv => {
            command.args([
                "-f", "flv", "-i", "pipe:0", "-map", "0:v:0?", "-map", "0:a:0?", "-c", "copy",
            ]);
        }
        InputFormat::BrowserWebm => {
            let gop = (segment_seconds * 30).to_string();
            // FFmpeg's multi-megabyte default probe waits until a Firefox
            // MediaRecorder stream ends: its WebM chunks are only ~80 KiB/s.
            // WebM declares its tracks in the first header, so 256 KiB is
            // enough for bounded, live startup without waiting for EOF.
            // Canvas capture is variable-rate; WebM may advertise a 1/1000
            // time base as 1000 fps. Emit a real 30 fps output for both HLS
            // and replay rather than failing replay validation on metadata.
            command.args(["-probesize", "262144", "-f", "matroska", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0?",
                "-vf", "scale=w='min(1280,iw)':h='min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2",
                "-r", "30", "-fps_mode", "cfr", "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                "-pix_fmt", "yuv420p", "-g", &gop, "-b:v", "2500k", "-maxrate", "2500k",
                "-bufsize", "5000k", "-c:a", "aac", "-b:a", "128k"]);
        }
        InputFormat::BrowserMp4 => {
            command.args([
                "-f", "mp4", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0?", "-c", "copy",
            ]);
        }
    }
    // Tee *after* the one required browser encode (or RTMP stream copy).
    // Relative slave paths avoid interpreting an operator's media path as
    // FFmpeg tee/muxer syntax. Fragmented MP4 commits each keyframe, leaving
    // a decodable prefix for restart recovery without a second live encode.
    command.args(["-flags", "+global_header", "-f", "tee", &tee_output]);
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut remux_failed = false;
    let mut recording_limit_reached = false;
    let mut disk_floor_reached = false;
    if let Ok(mut process) = child {
        if let Some(mut stdin) = process.stdin.take() {
            let header: &[u8] = if matches!(format, InputFormat::Flv) {
                b"FLV\x01\x05\0\0\0\x09\0\0\0\0"
            } else {
                &[]
            };
            if stdin.write_all(header).await.is_ok() {
                let mut last_size_check = tokio::time::Instant::now();
                let mut credential_check = tokio::time::interval(std::time::Duration::from_secs(2));
                credential_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let next = tokio::select! {
                        tag = rx.recv() => tag,
                        _ = credential_check.tick(), if session.ingest_key_generation.is_some() => {
                            match plamenu_db::native_live::ingest_key_current(&state.pool, &session).await {
                                Ok(true) => continue,
                                Ok(false) => {
                                    control.revoked.store(true, Ordering::Release);
                                    control.stopped.store(true, Ordering::Release);
                                    if let Some(socket) = &control.disconnect {
                                        let _ = socket.shutdown(Shutdown::Both);
                                    }
                                    tracing::info!(media = session.media_id, "revoked live stream key stopped publisher");
                                    break;
                                }
                                Err(error) => {
                                    remux_failed = true;
                                    tracing::warn!(%error, media = session.media_id, "cannot verify live stream key");
                                    break;
                                }
                            }
                        }
                    };
                    let Some(tag) = next else { break };
                    if stdin.write_all(&tag).await.is_err() {
                        remux_failed = true;
                        break;
                    }
                    if last_size_check.elapsed().as_secs() >= 1 {
                        last_size_check = tokio::time::Instant::now();
                        let disk_floor = state.config.native_video.min_free_gib
                            * crate::config::NativeVideoConfig::GIB
                            + 8 * 1024 * 1024;
                        if !matches!(crate::storage::available_bytes(&output), Ok(free) if free >= disk_floor)
                        {
                            disk_floor_reached = true;
                            control.stopped.store(true, Ordering::Release);
                            break;
                        }
                        let cap = state.config.native_video.max_upload_bytes();
                        let margin = (cap / 4).min(8 * 1024 * 1024);
                        if tokio::fs::metadata(&replay)
                            .await
                            .is_ok_and(|meta| meta.len() >= cap.saturating_sub(margin))
                        {
                            recording_limit_reached = true;
                            control.stopped.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            }
            drop(stdin);
            rx.close();
        }
        match process.wait_with_output().await {
            Ok(result) if !result.status.success() => {
                remux_failed = true;
                tracing::warn!(media = session.media_id, stderr = %String::from_utf8_lossy(&result.stderr), "live remux stopped");
            }
            Err(error) => {
                remux_failed = true;
                tracing::warn!(%error, media = session.media_id, "live remux wait failed");
            }
            _ => {}
        }
    } else if let Err(error) = child {
        remux_failed = true;
        tracing::error!(%error, media = session.media_id, "cannot start native live remux");
    }
    let reason = finish_reason(
        control.stopped.load(Ordering::Acquire),
        remux_failed,
        session.media_id,
    );
    if recording_limit_reached {
        tracing::warn!(
            media = session.media_id,
            "live broadcast stopped at replay size limit"
        );
    }
    if disk_floor_reached {
        tracing::warn!(
            media = session.media_id,
            "live broadcast stopped at media free-space floor"
        );
    }
    let closed = match plamenu_db::native_live::finish(&state.pool, &session, reason).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(error) => {
            tracing::error!(%error, media = session.media_id, "cannot finish live session");
            false
        }
    };
    let saveable = closed
        && tokio::fs::metadata(&replay)
            .await
            .is_ok_and(|metadata| metadata.len() > 0);
    let mut preserve_recording = false;
    if closed
        && session.permanent
        && let Err(error) = crate::native_live::federate_state(&state, session.status_id).await
    {
        tracing::warn!(%error, media = session.media_id, "could not announce reusable live stop");
    }
    if saveable && reason == FinishReason::Stopped {
        // Emit one final state. Two separately queued Updates could otherwise
        // arrive out of order and leave a peer on the ended placeholder.
        preserve_recording = !save_replay(&state, &session, &replay).await;
    }
    if closed
        && !session.permanent
        && let Err(error) = crate::native_live::federate_state(&state, session.status_id).await
    {
        tracing::warn!(%error, media = session.media_id, "could not announce live stop or replay");
    }
    if saveable && reason == FinishReason::Interrupted {
        // A transport can lose its final clean-close signal. Preserve its
        // recording across the one-minute reconnect window, then save it only
        // if its session remains unpublished. A later recurring session has
        // its own identity and does not supersede this recording.
        tokio::time::sleep(std::time::Duration::from_secs(70)).await;
        match plamenu_db::native_live::replay_eligible(&state.pool, &session).await {
            Ok(true) => {
                if save_replay(&state, &session, &replay).await {
                    if !session.permanent
                        && let Err(error) =
                            crate::native_live::federate_state(&state, session.status_id).await
                    {
                        tracing::warn!(%error, media = session.media_id, "could not announce live replay");
                    }
                } else {
                    preserve_recording = true;
                }
            }
            Ok(false) => {
                // A delayed deadline sweep can leave this generation
                // interrupted at the first check. Keep its fragments for a
                // restart instead of discarding a recoverable recording.
                preserve_recording = plamenu_db::native_live::recoverable_replays(&state.pool)
                    .await
                    .is_ok_and(|rows| {
                        rows.iter()
                            .any(|row| row.session.session_id == session.session_id)
                    });
            }
            Err(error) => {
                tracing::warn!(%error, media = session.media_id, "cannot check interrupted replay");
                preserve_recording = true;
            }
        }
    }
    // The sliding window is transient and never part of permanent media
    // storage. Give late player requests a brief drain window before deletion.
    tokio::time::sleep(std::time::Duration::from_secs(
        if saveable && reason == FinishReason::Interrupted {
            50
        } else {
            120
        },
    ))
    .await;
    if !preserve_recording && let Err(error) = tokio::fs::remove_dir_all(&output).await {
        tracing::debug!(%error, media = session.media_id, "live scratch cleanup");
    }
}

async fn save_replay(
    state: &AppState,
    session: &plamenu_db::native_live::ActiveSession,
    recording: &Path,
) -> bool {
    match crate::live_replay::finalize(state, session, recording).await {
        Ok(true) => true,
        Ok(false) => {
            tracing::warn!(
                media = session.media_id,
                "live replay finalization lost its session fence"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                media = session.media_id,
                "live replay could not be saved"
            );
            false
        }
    }
}

/// Resume an unpublished recording found after process restart. A reconnect
/// fences one-shot conversion, but recurring episodes remain independent.
pub async fn recover_crashed_replay(
    state: AppState,
    recovery: plamenu_db::native_live::ReplayRecovery,
) {
    let session = recovery.session;
    if !crate::routes::native_live_hls::valid_uuid(&session.session_id) {
        tracing::warn!(
            media = session.media_id,
            "invalid crashed live session identity"
        );
        return;
    }
    let output = session_dir(&state, &session);
    let recording = output.join("replay.mp4");
    // Startup marks an orphaned on-air session interrupted. Give its owner the
    // full reconnect window before converting it. The 10-second margin lets
    // the durable expiry sweep commit and announce ended first.
    let wait = recovery.reconnect_deadline.map_or(2, |deadline| {
        u64::try_from(
            (deadline - time::OffsetDateTime::now_utc())
                .whole_seconds()
                .max(0),
        )
        .unwrap_or(0)
        .saturating_add(10)
    });
    tokio::select! {
        () = state.shutdown.cancelled() => return,
        () = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
    }
    for _ in 0..12 {
        let candidates = match plamenu_db::native_live::recoverable_replays(&state.pool).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, media = session.media_id, "cannot inspect crashed live replay");
                return;
            }
        };
        if !candidates
            .iter()
            .any(|row| row.session.session_id == session.session_id)
        {
            // A reconnect, deletion or another finalizer superseded this
            // generation. It can never publish its stale recording.
            if let Err(error) = tokio::fs::remove_dir_all(&output).await {
                tracing::debug!(%error, media = session.media_id, "superseded live scratch cleanup");
            }
            return;
        }
        match plamenu_db::native_live::replay_eligible(&state.pool, &session).await {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, media = session.media_id, "cannot fence crashed live replay");
                return;
            }
        }
        tokio::select! {
            () = state.shutdown.cancelled() => return,
            () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
        }
    }
    if !matches!(
        plamenu_db::native_live::replay_eligible(&state.pool, &session).await,
        Ok(true)
    ) {
        tracing::warn!(
            media = session.media_id,
            "live replay is still awaiting its reconnect expiry"
        );
        return;
    }
    // An orphaned FFmpeg child may still be closing its pipe when the server
    // restarts. Fragments are salvageable, but do not process a moving file.
    let mut previous = 0;
    let mut stable = false;
    for _ in 0..10 {
        let bytes = tokio::fs::metadata(&recording)
            .await
            .map_or(0, |meta| meta.len());
        if bytes > 0 && bytes == previous {
            stable = true;
            break;
        }
        previous = bytes;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    if !stable {
        tracing::warn!(
            media = session.media_id,
            "crashed live recording has no stable fragment yet"
        );
        return;
    }
    if !save_replay(&state, &session, &recording).await {
        // Keep the scratch for a later retry/forensic salvage. Never claim a
        // successful replay or erase bytes after a failed finalization.
        return;
    }
    if !session.permanent
        && let Err(error) = crate::native_live::federate_state(&state, session.status_id).await
    {
        tracing::warn!(%error, media = session.media_id, "could not announce recovered live replay");
    }
    if let Err(error) = tokio::fs::remove_dir_all(&output).await {
        tracing::debug!(%error, media = session.media_id, "recovered live scratch cleanup");
    }
}

fn finish_reason(deliberate_stop: bool, remux_failed: bool, media_id: i64) -> FinishReason {
    if deliberate_stop && remux_failed {
        tracing::warn!(media_id, "live remux failed during a deliberate stop");
    }
    // A user-requested stop is intentional even when FFmpeg could not produce
    // a segment from a very short recording. Keep the transcoder warning, but
    // do not put that broadcast into the reconnect grace period.
    if deliberate_stop {
        FinishReason::Stopped
    } else {
        FinishReason::Interrupted
    }
}

#[must_use]
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run_rtmp(state).await {
            tracing::error!(%error, "native RTMP ingest stopped");
        }
    })
}

/// Keep a duplicate socket handle so key revocation can close a publisher's
/// transport. The upstream RTMP listener does not expose per-session close;
/// owning the accept loop lets its `Connection` observe EOF and perform its
/// normal registry/handler cleanup without a second port or process.
fn clonable_socket(socket: TcpStream) -> io::Result<(TcpStream, Arc<std::net::TcpStream>)> {
    let standard = socket.into_std()?;
    let disconnect = Arc::new(standard.try_clone()?);
    Ok((TcpStream::from_std(standard)?, disconnect))
}

async fn run_rtmp(state: AppState) -> io::Result<()> {
    let mut config = ServerConfig::default();
    config.bind_addr = state.config.live_ingest_bind;
    config.max_connections = 32;
    config.gop_buffer_enabled = false;
    let listener = TcpListener::bind(config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "native RTMP ingest listening");
    let registry = Arc::new(StreamRegistry::new());
    let cleanup = registry.spawn_cleanup_task();
    let handler = Arc::new(IngestHandler {
        state: state.clone(),
        publishers: Arc::default(),
        disconnects: Arc::default(),
    });
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let next_session = AtomicU64::new(1);
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (socket, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(%error, "RTMP accept failed");
                        continue;
                    }
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    tracing::warn!(%peer, "RTMP connection limit reached");
                    continue;
                };
                if config.tcp_nodelay && let Err(error) = socket.set_nodelay(true) {
                    tracing::warn!(%error, %peer, "RTMP socket setup failed");
                    continue;
                }
                let (socket, disconnect) = match clonable_socket(socket) {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(%error, %peer, "RTMP socket duplication failed");
                        continue;
                    }
                };
                let id = next_session.fetch_add(1, Ordering::Relaxed);
                handler.disconnects.lock().await.insert(id, disconnect);
                let handler = Arc::clone(&handler);
                let registry = Arc::clone(&registry);
                let config = config.clone();
                let shutdown = state.shutdown.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let mut connection = Connection::new(id, socket, peer, config, Arc::clone(&handler), registry);
                    let result = tokio::select! {
                        result = connection.run() => Some(result),
                        () = shutdown.cancelled() => None,
                    };
                    if let Some(Err(error)) = result {
                        tracing::debug!(%error, %peer, "RTMP publisher disconnected");
                    }
                    handler.disconnects.lock().await.remove(&id);
                    handler.close(id, false).await;
                });
            }
        }
    }
    cleanup.abort();
    Ok(())
}

fn rtmps_acceptor(config: &LiveRtmpsConfig) -> io::Result<TlsAcceptor> {
    let certs = CertificateDer::pem_file_iter(&config.cert)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .map(|item| item.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)))
        .collect::<io::Result<Vec<_>>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTMPS certificate chain is empty",
        ));
    }
    let key = PrivateKeyDer::from_pem_file(&config.key)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(TlsAcceptor::from(Arc::new(tls)))
}

/// Check certificate material before advertising an RTMPS endpoint or binding
/// the application. A broken initial TLS setup must not silently run as RTMP.
pub fn validate_rtmps(config: &Config) -> io::Result<()> {
    if let Some(rtmps) = &config.live_rtmps {
        rtmps_acceptor(rtmps)?;
    }
    Ok(())
}

/// TLS is terminated inside the Plamenu process and forwarded only to its
/// existing RTMP listener. There is one remux/session pipeline and no external
/// TLS or media daemon. Rotated certificates are picked up for new publishers.
#[must_use]
pub fn spawn_rtmps(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(config) = state.config.live_rtmps.clone() else {
            return;
        };
        let listener = match TcpListener::bind(config.bind).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::error!(%error, bind = %config.bind, "native RTMPS listener failed");
                return;
            }
        };
        let mut acceptor = match rtmps_acceptor(&config) {
            Ok(acceptor) => acceptor,
            Err(error) => {
                tracing::error!(%error, "native RTMPS certificate failed");
                return;
            }
        };
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(60));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let slots = Arc::new(Semaphore::new(32));
        let rtmp = state.config.live_ingest_bind;
        let upstream = SocketAddr::new(
            match rtmp.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                ip => ip,
            },
            rtmp.port(),
        );
        tracing::info!(bind = %config.bind, "native RTMPS ingest listening");
        loop {
            tokio::select! {
                () = state.shutdown.cancelled() => break,
                _ = refresh.tick() => {
                    match rtmps_acceptor(&config) {
                        Ok(updated) => acceptor = updated,
                        Err(error) => tracing::warn!(%error, "native RTMPS certificate reload failed; retaining previous certificate"),
                    }
                }
                accepted = listener.accept() => {
                    let Ok((socket, _)) = accepted else { continue };
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue };
                    let acceptor = acceptor.clone();
                    let shutdown = state.shutdown.clone();
                    tokio::spawn(async move {
                        let _slot = slot;
                        let session = async {
                            let mut tls = tokio::time::timeout(
                                std::time::Duration::from_secs(10), acceptor.accept(socket)
                            ).await??;
                            let mut plain = TcpStream::connect(upstream).await?;
                            plain.set_nodelay(true)?;
                            tokio::io::copy_bidirectional(&mut tls, &mut plain).await?;
                            Ok::<(), io::Error>(())
                        };
                        tokio::select! {
                            () = shutdown.cancelled() => {},
                            result = session => {
                                if let Err(error) = result {
                                    tracing::debug!(%error, "RTMPS publisher disconnected");
                                }
                            }
                        }
                    });
                }
            }
        }
    })
}

/// A durable deadline means interrupted streams resolve even if Plamenu
/// restarts during the reconnect window. Expiry uses the same single-writer
/// database and federation path as ordinary publisher transitions.
#[must_use]
pub fn spawn_reconnect_expiry(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = state.shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            match plamenu_db::native_live::expire_interruptions(&state.pool).await {
                Ok(status_ids) => {
                    for status_id in status_ids {
                        if let Err(error) =
                            crate::native_live::federate_state(&state, status_id).await
                        {
                            tracing::warn!(%error, status_id, "could not announce expired live interruption");
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "cannot expire live reconnect windows"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledged_stop_is_not_an_interruption_after_a_short_remux_failure() {
        assert_eq!(finish_reason(true, true, 0), FinishReason::Stopped);
        assert_eq!(finish_reason(true, false, 0), FinishReason::Stopped);
        assert_eq!(finish_reason(false, true, 0), FinishReason::Interrupted);
    }

    #[test]
    fn flv_tag_serialization() {
        let tag = FlvTag {
            tag_type: FlvTagType::Video,
            timestamp: 0x1234_5678,
            data: vec![0x17, 0x01].into(),
        };
        let bytes = serialize_tag(&tag).unwrap();
        assert_eq!(
            bytes,
            [
                9, 0, 0, 2, 0x34, 0x56, 0x78, 0x12, 0, 0, 0, 0x17, 0x01, 0, 0, 0, 13
            ]
        );
    }

    #[tokio::test]
    async fn crash_scratch_scavenger_only_removes_known_generations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("native-live").join("42");
        let generation = root.join("12345678-1234-1234-1234-123456789abc");
        let pending = root.join("12345678-1234-1234-1234-123456789def");
        let unrelated = root.join("operator-notes");
        tokio::fs::create_dir_all(&generation).await.unwrap();
        tokio::fs::create_dir_all(&pending).await.unwrap();
        tokio::fs::create_dir_all(&unrelated).await.unwrap();
        assert!(
            has_crashed_recording_dir(temp.path(), 42, "12345678-1234-1234-1234-123456789def")
                .await
        );
        assert!(!has_crashed_recording_dir(temp.path(), 42, "../operator-notes").await);
        let keep = HashSet::from([(42, "12345678-1234-1234-1234-123456789def".to_owned())]);
        assert_eq!(scavenge_crash_scratch(temp.path(), &keep).await.unwrap(), 1);
        assert!(!generation.exists());
        assert!(pending.exists());
        assert!(unrelated.exists());
    }
}
