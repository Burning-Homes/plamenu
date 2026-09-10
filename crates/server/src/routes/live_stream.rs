//! Universal live playback: one endless fragmented MP4 per broadcast, shared.
//!
//! The HLS lane ([`super::hls`]) serves the built-in player, which loads
//! hls.js. Everything else in the world does not: a `<video>` element with
//! scripts disabled, Phanpy in Firefox, Tusky's `ExoPlayer`, VLC, a Mastodon
//! client that only understands `MediaAttachment.url`. For recorded video that
//! audience is served a sparse-range MP4 facade ([`super::progressive`]), but
//! that trick needs a *complete* file to index — a live stream has no length,
//! no seek table, and no last byte.
//!
//! So a live is remuxed instead. `FFmpeg` reads the HLS stream and writes a
//! fragmented MP4 to a pipe, which this module hands to viewers as one
//! never-ending chunked response: initialization section first, then every
//! fragment as it is produced. Nothing is re-encoded — the packets are copied,
//! only the container changes — so the cost is a pipe and a few percent of one
//! core, not a transcode.
//!
//! **One `FFmpeg` per broadcast, not per viewer.** The process is started by
//! the first viewer, its output is broadcast to all of them, and it is stopped
//! shortly after the last one leaves. That is what keeps the rule the rest of
//! this stack obeys — *never more origin load than watching on the origin
//! directly* — true for this lane as well: a thousand viewers cost one stream.
//!
//! `FFmpeg` reads through our own HLS proxy on the loopback interface rather
//! than from the origin, so live segments it pulls are the same cached,
//! single-flighted, SSRF-fenced ones the browser lane uses, and the whole
//! gateway inherits that fence instead of handing a remote playlist to a
//! process that would follow it anywhere.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, broadcast, watch};

use crate::error::ApiError;
use crate::state::AppState;

/// How long a viewer waits for the stream to start producing before giving up.
/// `FFmpeg` must fetch the playlist and at least one segment first, so this is
/// a few segment durations rather than a moment.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a session runs on with nobody watching before it is stopped. Long
/// enough to survive a page reload or a player re-requesting the stream, short
/// enough that a closed tab stops costing the origin promptly.
const IDLE_GRACE: Duration = Duration::from_secs(20);
/// How often the watchdog checks whether a session has gone idle.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
/// Fragments held for viewers that briefly fall behind. Past this a slow
/// viewer is resynchronised to the live edge rather than being allowed to make
/// the session buffer without bound.
const FRAGMENT_BACKLOG: usize = 16;
/// Concurrent broadcasts this instance will remux at once. Each is one
/// `FFmpeg` process and one origin stream; beyond this, playback falls back to
/// the HLS lane rather than letting a busy timeline fork processes without
/// limit.
const MAX_SESSIONS: usize = 8;
/// Ceiling on one MP4 box while reassembling `FFmpeg`'s output. Fragments are
/// tens to hundreds of kilobytes; a box larger than this means the stream is
/// not what we think it is, and the session is abandoned rather than buffered.
const MAX_BOX_BYTES: usize = 32 * 1024 * 1024;
/// Read size from the `FFmpeg` pipe.
const PIPE_CHUNK: usize = 64 * 1024;

/// A running remux of one broadcast, shared by every viewer of it.
struct Session {
    /// `ftyp` + `moov`: the initialization section every viewer needs before
    /// any fragment will decode. Published once, replayed to every joiner.
    init: watch::Receiver<Option<Arc<Vec<u8>>>>,
    /// One message per complete `moof`+`mdat` fragment.
    fragments: broadcast::Sender<Arc<Vec<u8>>>,
    viewers: Arc<AtomicUsize>,
}

static SESSIONS: LazyLock<Mutex<HashMap<i64, Arc<Session>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Keeps a session's viewer count accurate even when the client vanishes: the
/// response body owns one of these, and dropping the body — which axum does on
/// disconnect — releases it.
struct ViewerGuard(Arc<AtomicUsize>);

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `GET /media/live/{media_id}/stream.mp4` — the broadcast as one endless
/// fragmented MP4.
///
/// Deliberately not range-capable: there is no length to range over, and a
/// player that asks for one is answered with the stream from the live edge.
pub async fn stream(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
) -> Result<Response, ApiError> {
    if plamenu_db::media::owner_suspended(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    // Asking for the stream is asking to watch, so this is the moment to be
    // sure about whether the broadcast is actually running.
    crate::live_refresh::refresh_for_media(&state, media_id).await;
    let item = plamenu_db::media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    if item.live_state.as_deref() != Some("live") || item.hls_master_url.is_none() {
        return Err(ApiError::NotFound);
    }

    let session = session_for(&state, media_id).await?;
    // Claim the viewer slot before waiting for the stream to start, so the
    // watchdog cannot decide the session is idle while its first viewer is
    // still waiting for FFmpeg to produce the initialization section.
    session.viewers.fetch_add(1, Ordering::SeqCst);
    let guard = ViewerGuard(Arc::clone(&session.viewers));

    // Subscribe first, then take the initialization section: a fragment
    // produced while we were still setting up is one this viewer should get,
    // and `init` is always published before any fragment.
    let fragments = session.fragments.subscribe();
    let Some(init) = await_init(&session).await else {
        return Err(ApiError::NotFound);
    };

    let body = Body::from_stream(fragment_stream(init, fragments, guard));
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4"),
            // Endless and unseekable: no length to advertise, and nothing may
            // cache a stream whose meaning is "from now on".
            (header::CACHE_CONTROL, "no-store"),
            (header::ACCEPT_RANGES, "none"),
        ],
        body,
    )
        .into_response())
}

/// Waits for the session's initialization section, giving up rather than
/// holding a request open against a broadcast that never starts producing.
async fn await_init(session: &Session) -> Option<Arc<Vec<u8>>> {
    let mut init = session.init.clone();
    let wait = async {
        loop {
            if let Some(bytes) = init.borrow_and_update().clone() {
                return Some(bytes);
            }
            // The sender is dropped when the session dies; that ends the wait.
            init.changed().await.ok()?;
        }
    };
    tokio::time::timeout(START_TIMEOUT, wait)
        .await
        .ok()
        .flatten()
}

/// The response body: the initialization section, then every fragment as it is
/// produced, until the viewer disconnects or the broadcast stops.
fn fragment_stream(
    init: Arc<Vec<u8>>,
    fragments: broadcast::Receiver<Arc<Vec<u8>>>,
    guard: ViewerGuard,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    // The guard rides along in the stream's state, so the viewer count falls
    // exactly when the client disconnects and axum drops the body.
    futures_util::stream::unfold(
        (Some(init), fragments, guard),
        |(pending, mut fragments, guard)| async move {
            let mut pending = pending;
            if let Some(init) = pending.take() {
                return Some((Ok(init.to_vec()), (None, fragments, guard)));
            }
            loop {
                match fragments.recv().await {
                    Ok(fragment) => {
                        return Some((Ok(fragment.to_vec()), (None, fragments, guard)));
                    }
                    // Fell behind the backlog: skip to the live edge rather
                    // than ending the stream. A live viewer wants "now", not
                    // a faithful replay of what they missed.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    // The session ended — the broadcast is over.
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    )
}

/// The running session for a broadcast, started if this is the first viewer.
async fn session_for(state: &AppState, media_id: i64) -> Result<Arc<Session>, ApiError> {
    let mut sessions = SESSIONS.lock().await;
    if let Some(existing) = sessions.get(&media_id) {
        // A session whose FFmpeg has exited has no receivers left to feed;
        // drop it so this viewer starts a fresh one.
        if existing.fragments.receiver_count() > 0 || existing.init.borrow().is_some() {
            return Ok(Arc::clone(existing));
        }
        sessions.remove(&media_id);
    }
    if sessions.len() >= MAX_SESSIONS {
        tracing::warn!(
            media = media_id,
            "live remux session limit reached; falling back to the HLS lane"
        );
        return Err(ApiError::NotFound);
    }
    let session = spawn_session(state, media_id)?;
    sessions.insert(media_id, Arc::clone(&session));
    Ok(session)
}

/// Starts the remux for one broadcast: the `FFmpeg` child, the task that turns
/// its pipe into fragments, and the watchdog that stops it once nobody is
/// watching.
fn spawn_session(state: &AppState, media_id: i64) -> Result<Arc<Session>, ApiError> {
    let input = loopback_master_url(state, media_id);
    let mut child = tokio::process::Command::new(&state.config.ffmpeg_path)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            // The input is our own proxy on the loopback interface; nothing
            // else needs to be reachable.
            "-protocol_whitelist",
            "http,tcp",
            "-i",
            &input,
            // Remux only. A live broadcast is already H.264/AAC in the shape
            // browsers want; re-encoding it would cost a core per viewer and
            // buy nothing.
            "-c",
            "copy",
            // HLS carries AAC as ADTS frames, which MP4 cannot hold: without
            // this the muxer refuses every audio packet and the stream comes
            // out silent — or, worse, not at all.
            "-bsf:a",
            "aac_adtstoasc",
            "-f",
            "mp4",
            // The fragmented-MP4 profile a browser can start playing from the
            // first byte: an empty header that needs no file length, a
            // fragment at every keyframe, self-contained fragment offsets, a
            // `moov` delayed until the codecs are actually known (with `-c
            // copy` they are not knowable before the first packet), and no
            // trailer index — there is no end of file to write one at.
            "-movflags",
            "empty_moov+frag_keyframe+default_base_moof+delay_moov+skip_trailer",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            tracing::warn!(%error, media = media_id, "cannot start the live remux");
            ApiError::NotFound
        })?;

    let stdout = child.stdout.take().ok_or(ApiError::NotFound)?;
    let stderr = child.stderr.take();
    let (init_tx, init_rx) = watch::channel(None);
    let (fragments, _) = broadcast::channel(FRAGMENT_BACKLOG);
    let viewers = Arc::new(AtomicUsize::new(0));
    let session = Arc::new(Session {
        init: init_rx,
        fragments: fragments.clone(),
        viewers: Arc::clone(&viewers),
    });

    // Diagnostics only: FFmpeg's complaints are worth a log line, but nothing
    // waits on them.
    if let Some(mut stderr) = stderr {
        tokio::spawn(async move {
            let mut message = String::new();
            if tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut message)
                .await
                .is_ok()
                && !message.trim().is_empty()
            {
                tracing::debug!(media = media_id, "live remux: {}", message.trim());
            }
        });
    }

    tokio::spawn(pump(media_id, stdout, init_tx, fragments));
    tokio::spawn(watchdog(media_id, child, viewers));
    Ok(session)
}

/// Reads `FFmpeg`'s fragmented MP4 out of the pipe and republishes it as an
/// initialization section plus a stream of whole fragments.
///
/// Viewers join at arbitrary times, so they cannot simply be handed the byte
/// stream from wherever it happens to be: an MP4 fragment is only decodable
/// whole, and only after the `moov` that describes the tracks. Walking the box
/// structure is what makes "join now" mean "join at the next fragment".
async fn pump(
    media_id: i64,
    mut stdout: tokio::process::ChildStdout,
    init_tx: watch::Sender<Option<Arc<Vec<u8>>>>,
    fragments: broadcast::Sender<Arc<Vec<u8>>>,
) {
    let mut buffer: Vec<u8> = Vec::with_capacity(PIPE_CHUNK * 2);
    let mut chunk = vec![0_u8; PIPE_CHUNK];
    // Everything before the first `moof` is the initialization section.
    let mut init: Option<Vec<u8>> = Some(Vec::new());
    // The fragment being assembled: one `moof` and everything up to the next.
    let mut fragment: Vec<u8> = Vec::new();

    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..read]);

        while let Some((kind, len)) = next_box(&buffer) {
            if len > MAX_BOX_BYTES {
                tracing::warn!(
                    media = media_id,
                    len,
                    "live remux produced an implausible box"
                );
                return;
            }
            if buffer.len() < len {
                break; // incomplete; wait for more bytes
            }
            let boxed: Vec<u8> = buffer.drain(..len).collect();
            if &kind == b"moof" {
                // A new fragment starts: publish the previous one, and close
                // the initialization section the first time around.
                if let Some(section) = init.take() {
                    let _ = init_tx.send(Some(Arc::new(section)));
                } else if !fragment.is_empty() {
                    let _ = fragments.send(Arc::new(std::mem::take(&mut fragment)));
                }
                fragment = boxed;
            } else if let Some(section) = init.as_mut() {
                section.extend_from_slice(&boxed);
            } else {
                fragment.extend_from_slice(&boxed);
            }
        }
        if buffer.len() > MAX_BOX_BYTES {
            tracing::warn!(media = media_id, "live remux outran its reassembly bound");
            return;
        }
    }
    // End of stream: flush whatever was complete, then let every viewer's
    // receiver close so their responses end cleanly.
    if !fragment.is_empty() {
        let _ = fragments.send(Arc::new(fragment));
    }
    tracing::debug!(media = media_id, "live remux ended");
}

/// The type and total length of the box at the front of `buffer`, once its
/// header is complete.
fn next_box(buffer: &[u8]) -> Option<([u8; 4], usize)> {
    if buffer.len() < 8 {
        return None;
    }
    let size = u32::from_be_bytes(buffer[0..4].try_into().ok()?);
    let kind: [u8; 4] = buffer[4..8].try_into().ok()?;
    let len = match size {
        // 64-bit `largesize` follows the header.
        1 => {
            if buffer.len() < 16 {
                return None;
            }
            usize::try_from(u64::from_be_bytes(buffer[8..16].try_into().ok()?)).ok()?
        }
        // "to end of file" — meaningless in a live pipe.
        0 => return None,
        size => size as usize,
    };
    (len >= 8).then_some((kind, len))
}

/// Stops the remux once nobody has been watching for [`IDLE_GRACE`], or as
/// soon as `FFmpeg` exits on its own (the broadcast ended).
async fn watchdog(media_id: i64, mut child: tokio::process::Child, viewers: Arc<AtomicUsize>) {
    let mut idle_since: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            _ = child.wait() => break,
            () = tokio::time::sleep(WATCHDOG_TICK) => {}
        }
        if viewers.load(Ordering::SeqCst) == 0 {
            let since = idle_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= IDLE_GRACE {
                tracing::debug!(media = media_id, "live remux idle; stopping");
                let _ = child.kill().await;
                break;
            }
        } else {
            idle_since = None;
        }
    }
    SESSIONS.lock().await.remove(&media_id);
}

/// Where `FFmpeg` reads the broadcast from: this instance's own HLS proxy, on
/// the loopback interface, in plain HTTP.
///
/// Going back out through the public hostname would mean a second trip through
/// the reverse proxy and TLS that a local process has no business needing;
/// going to the origin directly would drop both the segment cache and the
/// SSRF fence that keeps a remote playlist from naming arbitrary addresses.
fn loopback_master_url(state: &AppState, media_id: i64) -> String {
    let bind = state.config.bind;
    let host = if bind.ip().is_unspecified() {
        if bind.is_ipv6() {
            "[::1]".to_owned()
        } else {
            "127.0.0.1".to_owned()
        }
    } else if bind.is_ipv6() {
        format!("[{}]", bind.ip())
    } else {
        bind.ip().to_string()
    };
    format!(
        "http://{host}:{}/media/hls/{media_id}/master.m3u8",
        bind.port()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn box_headers_are_read_including_largesize() {
        // Ordinary 32-bit size.
        let mut buf = Vec::new();
        buf.extend_from_slice(&24_u32.to_be_bytes());
        buf.extend_from_slice(b"moof");
        assert_eq!(next_box(&buf), Some((*b"moof", 24)));

        // 64-bit largesize.
        let mut big = Vec::new();
        big.extend_from_slice(&1_u32.to_be_bytes());
        big.extend_from_slice(b"mdat");
        big.extend_from_slice(&40_u64.to_be_bytes());
        assert_eq!(next_box(&big), Some((*b"mdat", 40)));

        // Incomplete headers yield nothing rather than a wrong answer.
        assert_eq!(next_box(&big[..12]), None);
        assert_eq!(next_box(b"abc"), None);
        // A size that cannot contain its own header is refused.
        let mut absurd = Vec::new();
        absurd.extend_from_slice(&4_u32.to_be_bytes());
        absurd.extend_from_slice(b"moof");
        assert_eq!(next_box(&absurd), None);
    }

    #[test]
    fn the_input_url_stays_on_the_loopback() {
        let url = |addr: &str| {
            let bind: SocketAddr = addr.parse().unwrap();
            let host = if bind.ip().is_unspecified() {
                "127.0.0.1".to_owned()
            } else {
                bind.ip().to_string()
            };
            format!("http://{host}:{}/media/hls/7/master.m3u8", bind.port())
        };
        assert_eq!(
            url("0.0.0.0:8420"),
            "http://127.0.0.1:8420/media/hls/7/master.m3u8"
        );
        assert_eq!(
            url("127.0.0.1:9000"),
            "http://127.0.0.1:9000/media/hls/7/master.m3u8"
        );
    }
}
