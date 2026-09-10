//! Caching HLS reverse-proxy for HLS-native remote video (`PeerTube`).
//!
//! `PeerTube` publishes a `Video` as an HLS master playlist plus, per rendition,
//! a single fragmented mp4 addressed by `#EXT-X-BYTERANGE` (segments are byte
//! ranges into that one file). Rather than download whole files, we mirror the
//! actual wire structure: fetch + rewrite the playlists so every child URI
//! points back through this instance, and serve each segment's byte range from
//! a per-range cache. That means a viewer who disengages after 5 s only ever
//! caused the handful of segments they watched to be fetched, and N viewers of
//! the same segment share ONE origin fetch — strictly less origin load than
//! watching on `PeerTube` directly. See `PEERTUBE_HLS_DESIGN.md`.
//!
//! The playlists we serve are plain multi-segment HLS: `PeerTube`'s
//! `#EXT-X-BYTERANGE` is folded into each segment's proxied URL (as `s`/`l`
//! query params), so the browser fetches distinct, immutable, cacheable segment
//! URLs and we never need the origin's total file length.
//!
//! A **live** broadcast comes through the same routes but is the opposite
//! shape in every respect that matters here: standalone MPEG-TS segment files
//! instead of byte ranges into one fMP4, a sliding window whose older segments
//! the origin deletes, and a playlist that is rewritten every few seconds
//! instead of being fixed for the life of the video. Each layer therefore
//! branches on [`Lane::live`]: range-less segment requests are accepted,
//! playlists are barely memoized and never client-cached, and segments go to
//! the ephemeral in-memory cache in [`super::hls_live`] rather than the
//! permanent on-disk ledger.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use plamenu_db::{media, media_fetch_failure};
use serde::Deserialize;

use super::hls_live;
use crate::error::ApiError;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

/// Cache policy for a rewritten playlist: short — the ladder is stable but the
/// rewrite is cheap and a stale one must not outlive an origin change.
const PLAYLIST_CACHE: &str = "public, max-age=60";
/// Cache policy for a *live* playlist. A live media playlist is a sliding
/// window rewritten by the origin every few seconds, and a player that is told
/// to reuse a stale one falls off the end of the window and stalls.
const LIVE_PLAYLIST_CACHE: &str = "no-store";
/// Cache policy for a served segment: immutable — a byte range never changes.
const SEGMENT_CACHE: &str = "public, max-age=31536000, immutable";
/// `application/vnd.apple.mpegurl` — the HLS playlist media type hls.js expects.
const M3U8: &str = "application/vnd.apple.mpegurl";
/// Ceiling on ONE segment's byte range. A segment is a few seconds of video (a
/// few MB even at high bitrate); this bounds a malicious/buggy origin that
/// advertises a giant `#EXT-X-BYTERANGE` from making us fetch + store a huge
/// "segment" (disk/bandwidth `DoS`).
const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Single-flight: concurrent first-plays of the SAME segment coalesce to one
// origin fetch. Keyed by the cache key string. (Never construct/drop the guard
// while holding the lock — the freeze lesson; `then` is lazy.)
// ---------------------------------------------------------------------------
static SEG_INFLIGHT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

struct SegGuard(String);

impl SegGuard {
    fn begin(key: &str) -> Option<Self> {
        let inserted = SEG_INFLIGHT.lock_or_recover().insert(key.to_owned());
        inserted.then(|| Self(key.to_owned()))
    }
    fn running(key: &str) -> bool {
        SEG_INFLIGHT.lock_or_recover().contains(key)
    }
}

impl Drop for SegGuard {
    fn drop(&mut self) {
        SEG_INFLIGHT.lock_or_recover().remove(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Persistent dead-origin suppression. Each HLS lane has a finite lifetime
// failure budget; process restarts do not reset it, and no timer retries after
// exhaustion.
// ---------------------------------------------------------------------------
const HLS_FAIL_COOLDOWN: Duration = Duration::from_secs(30);

async fn hls_suppressed(state: &AppState, kind: &str, media_id: i64) -> Result<bool, ApiError> {
    Ok(
        media_fetch_failure::is_cooling_down(&state.pool, kind, media_id, HLS_FAIL_COOLDOWN)
            .await?,
    )
}

async fn note_hls_result(state: &AppState, kind: &str, media_id: i64, success: bool) {
    let result = if success {
        media_fetch_failure::clear(&state.pool, kind, media_id).await
    } else {
        media_fetch_failure::record(&state.pool, kind, media_id).await
    };
    if let Err(error) = result {
        tracing::warn!(%error, kind, media = media_id, "failed to persist HLS fetch outcome");
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PlQuery {
    u: String,
}

#[derive(Deserialize)]
pub struct SegQuery {
    u: String,
    s: Option<u64>,
    l: Option<u64>,
}

/// `GET /media/hls/{media_id}/master.m3u8` — the player's entry point: fetch
/// the origin master playlist and serve a rewritten copy whose rendition and
/// audio-group URIs all point back through this proxy.
pub async fn master(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
) -> Result<Response, ApiError> {
    // Reaching for the master is someone pressing play. For a broadcast that
    // is the moment to be exact about whether it is running: a stream that
    // started since this page was rendered must play, and one that ended must
    // say so rather than serve a playlist whose segments are already gone.
    let lane = lane(&state, media_id).await?;
    let lane = if lane.live {
        crate::live_refresh::refresh_for_media(&state, media_id).await;
        lane_reload(&state, media_id).await?
    } else {
        lane
    };
    if lane.live && !lane.on_air {
        return Err(ApiError::NotFound);
    }
    let master_url = lane.master_url;
    if hls_disabled(&state).await? {
        return Err(ApiError::NotFound);
    }
    // Recent master failure → fail fast, don't re-probe a dead/hostile origin.
    if hls_suppressed(&state, "hls-master", media_id).await? {
        return Err(ApiError::NotFound);
    }
    match serve_playlist(&state, media_id, &master_url, lane.live).await {
        Ok(response) => {
            note_hls_result(&state, "hls-master", media_id, true).await;
            Ok(response)
        }
        Err(err) => {
            note_hls_result(&state, "hls-master", media_id, false).await;
            Err(err)
        }
    }
}

/// `GET /media/hls/{media_id}/pl?u=…` — a nested media playlist (a rendition,
/// or the separated audio track) referenced by the master.
pub async fn playlist(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(q): Query<PlQuery>,
) -> Result<Response, ApiError> {
    let lane = lane(&state, media_id).await?;
    let url = decode_child(&q.u, &dir_prefix(&lane.master_url)).ok_or(ApiError::NotFound)?;
    if hls_suppressed(&state, "hls-playlist", media_id).await? {
        return Err(ApiError::NotFound);
    }
    let result = serve_playlist(&state, media_id, &url, lane.live).await;
    note_hls_result(&state, "hls-playlist", media_id, result.is_ok()).await;
    result
}

/// `GET /media/hls/{media_id}/seg?u=…&s=…&l=…` — one byte range of a
/// rendition's fragmented mp4 (an HLS media segment or the fMP4 init segment),
/// served from the per-range cache and fetched from the origin once on a miss.
pub async fn segment(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(q): Query<SegQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let lane = lane(&state, media_id).await?;
    let origin = decode_child(&q.u, &dir_prefix(&lane.master_url)).ok_or(ApiError::NotFound)?;
    if hls_suppressed(&state, "hls-segment", media_id).await? {
        return Err(ApiError::NotFound);
    }
    // A live segment is a whole small file of its own, so it carries no range;
    // a VOD segment is always a byte range into one big rendition file, and a
    // range-less request for one would mean a whole-rendition download, which
    // this proxy never does. A range over the cap is a malicious or broken
    // origin either way.
    let result = match (q.s, q.l) {
        (Some(start), Some(len)) if len > 0 && len <= MAX_SEGMENT_BYTES => {
            serve_segment(&state, media_id, &origin, start, len, &headers).await
        }
        (None, None) if lane.live => serve_live_segment(&state, media_id, &origin).await,
        _ => Err(ApiError::NotFound),
    };
    note_hls_result(&state, "hls-segment", media_id, result.is_ok()).await;
    result
}

/// `GET /media/hls/{media_id}/live/{name}` — a live broadcast's child
/// resource, where `name` is the base64url origin URL with that origin's own
/// file extension appended.
///
/// One route for both playlists and segments, because what a live child *is*
/// follows from the origin URL we just decoded, not from how the request was
/// spelled. The extension exists purely so players and demuxers that infer a
/// container from the path get the right answer (see [`live_child_url`]).
pub async fn live_child(
    State(state): State<AppState>,
    Path((media_id, name)): Path<(i64, String)>,
) -> Result<Response, ApiError> {
    let lane = lane(&state, media_id).await?;
    if !lane.live {
        return Err(ApiError::NotFound);
    }
    // Strip the extension we appended; base64url contains no dots, so the
    // last one is always the separator we added.
    let encoded = name
        .rsplit_once('.')
        .map_or(name.as_str(), |(head, _)| head);
    let origin = decode_child(encoded, &dir_prefix(&lane.master_url)).ok_or(ApiError::NotFound)?;
    let kind = if is_m3u8(&origin) {
        "hls-playlist"
    } else {
        "hls-segment"
    };
    if hls_suppressed(&state, kind, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let result = if is_m3u8(&origin) {
        serve_playlist(&state, media_id, &origin, true).await
    } else {
        serve_live_segment(&state, media_id, &origin).await
    };
    note_hls_result(&state, kind, media_id, result.is_ok()).await;
    result
}

// ---------------------------------------------------------------------------
// Playlist fetch + rewrite
// ---------------------------------------------------------------------------

/// Rewritten playlists memoized for exactly the 60 s the browser is told to
/// cache them ([`PLAYLIST_CACHE`]): N concurrent viewers of one video cost
/// the origin one playlist fetch per minute, not one per viewer per minute.
/// Bounded — a playlist is a few KB, so the cap keeps this at single-digit
/// MB; when full, new entries simply go unmemoized until a sweep frees room.
const PLAYLIST_TTL: Duration = Duration::from_mins(1);
/// The same idea for a live playlist, but scaled to how fast it actually
/// changes: long enough that a burst of viewers still costs one origin fetch,
/// short enough that nobody is handed a window the origin has already moved on
/// from.
const LIVE_PLAYLIST_TTL: Duration = Duration::from_secs(1);
const PLAYLIST_MEMO_MAX: usize = 256;

/// (media id, origin url) → (memoized at, rewritten body).
type PlaylistMemo = HashMap<(i64, String), (Instant, String)>;

static PLAYLIST_MEMO: LazyLock<Mutex<PlaylistMemo>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn playlist_memo_get(media_id: i64, url: &str, live: bool) -> Option<String> {
    let ttl = if live {
        LIVE_PLAYLIST_TTL
    } else {
        PLAYLIST_TTL
    };
    let memo = PLAYLIST_MEMO.lock_or_recover();
    memo.get(&(media_id, url.to_owned()))
        .filter(|(cached_at, _)| cached_at.elapsed() < ttl)
        .map(|(_, body)| body.clone())
}

fn playlist_memo_put(media_id: i64, url: &str, body: &str) {
    let mut memo = PLAYLIST_MEMO.lock_or_recover();
    if memo.len() >= PLAYLIST_MEMO_MAX {
        memo.retain(|_, (cached_at, _)| cached_at.elapsed() < PLAYLIST_TTL);
        if memo.len() >= PLAYLIST_MEMO_MAX {
            return;
        }
    }
    memo.insert(
        (media_id, url.to_owned()),
        (Instant::now(), body.to_owned()),
    );
}

async fn serve_playlist(
    state: &AppState,
    media_id: i64,
    url: &str,
    live: bool,
) -> Result<Response, ApiError> {
    if let Some(memoized) = playlist_memo_get(media_id, url, live) {
        return Ok(playlist_response(memoized, live));
    }
    // Coalesce concurrent cold fetches of the same playlist (the same
    // single-flight set the segments use, distinct key space).
    let key = format!("pl:{url}");
    let guard = SegGuard::begin(&key);
    if guard.is_none() {
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(memoized) = playlist_memo_get(media_id, url, live) {
                return Ok(playlist_response(memoized, live));
            }
            if !SegGuard::running(&key) {
                break; // the winner finished (or died) unmemoized
            }
        }
        // Re-check, then fall through to fetching ourselves.
        if let Some(memoized) = playlist_memo_get(media_id, url, live) {
            return Ok(playlist_response(memoized, live));
        }
    }
    let fetched = state
        .federation
        .fetch_media(url)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let body = String::from_utf8_lossy(&fetched.bytes);
    // ffmpeg restarts every broadcast's media sequence at zero while a
    // permanent live keeps its master URL *and* its segment filenames, so a
    // sequence that went backwards is the one reliable signal that cached
    // bytes now belong to a stream nobody is watching any more.
    if live
        && let Some(sequence) = hls_live::media_sequence(&body)
        && hls_live::sequence_restarted(media_id, url, sequence)
    {
        tracing::debug!(
            media = media_id,
            sequence,
            "live session restarted; dropping cache"
        );
        hls_live::purge_media(media_id);
    }
    let rewritten = rewrite_playlist(&body, url, media_id, live);
    playlist_memo_put(media_id, url, &rewritten);
    Ok(playlist_response(rewritten, live))
}

fn playlist_response(rewritten: String, live: bool) -> Response {
    let cache = if live {
        LIVE_PLAYLIST_CACHE
    } else {
        PLAYLIST_CACHE
    };
    (
        [(header::CONTENT_TYPE, M3U8), (header::CACHE_CONTROL, cache)],
        rewritten,
    )
        .into_response()
}

/// Rewrites an HLS playlist so every child URI (nested playlists, segments, the
/// init segment) points back through this proxy. `#EXT-X-BYTERANGE` is folded
/// into each segment's proxied URL and dropped, turning a single-file-fMP4
/// playlist into a plain multi-segment one; everything else
/// (`BANDWIDTH`/`RESOLUTION`/`CODECS`/`EXTINF`/…) is passed through verbatim.
fn rewrite_playlist(body: &str, base_url: &str, media_id: i64, is_live: bool) -> String {
    let child = |abs: &str| {
        if is_live {
            live_child_url(media_id, abs)
        } else if is_m3u8(abs) {
            pl_url(media_id, abs)
        } else {
            seg_url(media_id, abs, None)
        }
    };
    let base = url::Url::parse(base_url).ok();
    let mut out = String::with_capacity(body.len() * 2);
    // For single-file fMP4, a segment's byte range is on the preceding
    // EXT-X-BYTERANGE line; `next_offset` continues an omitted `@offset`.
    let mut pending_range: Option<(u64, u64)> = None;
    let mut next_offset: u64 = 0;
    for raw in body.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some((rewritten, end)) = rewrite_map(rest, base.as_ref(), media_id) {
                next_offset = end;
                out.push_str(&rewritten);
            } else {
                out.push_str(line);
            }
            out.push('\n');
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let (start, len) = parse_byterange(rest, next_offset);
            next_offset = start.saturating_add(len);
            pending_range = Some((start, len));
            continue; // folded into the following segment URL
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            out.push_str("#EXT-X-MEDIA:");
            out.push_str(&rewrite_attr_uri(rest, base.as_ref(), media_id, is_live));
            out.push('\n');
            continue;
        }
        if line.starts_with('#') || line.is_empty() {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // A bare URI line: a child media playlist (.m3u8) or a segment.
        match resolve(base.as_ref(), line) {
            Some(abs) if is_live => out.push_str(&child(&abs)),
            Some(abs) if is_m3u8(&abs) => out.push_str(&pl_url(media_id, &abs)),
            Some(abs) => {
                if let Some((start, len)) = pending_range {
                    out.push_str(&seg_url(media_id, &abs, Some((start, len))));
                } else {
                    out.push_str(&seg_url(media_id, &abs, None));
                }
            }
            None => out.push_str(line),
        }
        out.push('\n');
        pending_range = None;
    }
    out
}

/// Rewrites `#EXT-X-MAP:URI="file",BYTERANGE="len@off"` into a proxied,
/// range-carrying URI (dropping `BYTERANGE`). Returns the rewritten line and
/// the byte just past the init segment (for the following segment's offset).
fn rewrite_map(attrs: &str, base: Option<&url::Url>, media_id: i64) -> Option<(String, u64)> {
    let uri = attr(attrs, "URI")?;
    let abs = resolve(base, &uri)?;
    let (start, len) = attr(attrs, "BYTERANGE").map_or((0, 0), |b| parse_byterange(&b, 0));
    let proxied = seg_url(media_id, &abs, (len > 0).then_some((start, len)));
    Some((
        format!("#EXT-X-MAP:URI=\"{proxied}\""),
        start.saturating_add(len),
    ))
}

/// Rewrites the `URI="…"` attribute inside an `#EXT-X-MEDIA` (audio/subtitle
/// group) line, leaving all other attributes verbatim. Routes `.m3u8` children
/// to `pl`, everything else to `seg`.
fn rewrite_attr_uri(attrs: &str, base: Option<&url::Url>, media_id: i64, live: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for part in split_attrs(attrs) {
        let uri = part
            .split_once('=')
            .filter(|(k, _)| k.trim() == "URI")
            .and_then(|(_, v)| resolve(base, v.trim().trim_matches('"')));
        if let Some(abs) = uri {
            let proxied = if live {
                live_child_url(media_id, &abs)
            } else if is_m3u8(&abs) {
                pl_url(media_id, &abs)
            } else {
                seg_url(media_id, &abs, None)
            };
            parts.push(format!("URI=\"{proxied}\""));
        } else {
            parts.push(part.to_owned());
        }
    }
    parts.join(",")
}

// ---------------------------------------------------------------------------
// Segment cache
// ---------------------------------------------------------------------------

async fn serve_segment(
    state: &AppState,
    media_id: i64,
    origin: &str,
    start: u64,
    len: u64,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let start_i = i64::try_from(start).map_err(|_| ApiError::NotFound)?;
    let len_i = i64::try_from(len).map_err(|_| ApiError::NotFound)?;

    if let Some(hit) = media::hls_segment_lookup(&state.pool, origin, start_i, len_i).await? {
        match super::media::stream_stored_file(
            state,
            &hit.cache_file,
            "video/mp4",
            SEGMENT_CACHE,
            headers,
        )
        .await
        {
            // The row outlived its file (lost disk, out-of-band deletion):
            // forget it and refetch below, instead of 404ing until the
            // failure backoff makes the whole video unplayable.
            Err(ApiError::NotFound) => {
                tracing::warn!(
                    origin,
                    start,
                    len,
                    cache_file = %hit.cache_file,
                    "cached HLS segment file missing; forgetting row and refetching"
                );
                media::hls_segment_forget(&state.pool, origin, start_i, len_i).await?;
            }
            served => {
                media::hls_segment_touch(&state.pool, origin, start_i, len_i)
                    .await
                    .ok();
                return served;
            }
        }
    }

    let cache_file = fetch_and_cache_segment(state, media_id, origin, start, len).await?;
    media::hls_segment_touch(&state.pool, origin, start_i, len_i)
        .await
        .ok();
    super::media::stream_stored_file(state, &cache_file, "video/mp4", SEGMENT_CACHE, headers).await
}

/// Serves one whole live segment — a standalone MPEG-TS file, not a byte range
/// — from the bounded in-memory cache, fetching it from the origin on a miss.
///
/// Concurrent viewers at the live edge all want the same segment within the
/// same couple of seconds, so this is where the "N viewers, one origin fetch"
/// rule is actually earned: the first request through fetches, the rest wait
/// briefly for it to publish rather than each hitting the origin.
async fn serve_live_segment(
    state: &AppState,
    media_id: i64,
    origin: &str,
) -> Result<Response, ApiError> {
    let content_type = hls_live::segment_content_type(origin);
    if let Some(hit) = hls_live::get(origin) {
        return Ok(live_segment_response(&hit, content_type));
    }
    if hls_disabled(state).await? {
        return Err(ApiError::NotFound);
    }

    let key = format!("live:{origin}");
    let guard = SegGuard::begin(&key);
    if guard.is_none() {
        // A live segment is seconds of video, so a waiter that gives up has
        // lost the race to be useful anyway; bound the wait tightly and fall
        // through to fetching rather than stalling the player.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(hit) = hls_live::get(origin) {
                return Ok(live_segment_response(&hit, content_type));
            }
            if !SegGuard::running(&key) {
                break; // the winner finished (or died) without publishing
            }
        }
    }
    if let Some(hit) = hls_live::get(origin) {
        return Ok(live_segment_response(&hit, content_type));
    }

    let fetched = state
        .federation
        .fetch_media(origin)
        .await
        .map_err(|error| {
            tracing::warn!(%error, origin, "live HLS segment origin fetch failed");
            ApiError::NotFound
        })?;
    let bytes = std::sync::Arc::new(fetched.bytes.clone());
    // Oversized segments are still served, just never remembered — `put`
    // refuses them so one broken origin cannot evict every other broadcast.
    hls_live::put(media_id, origin, std::sync::Arc::clone(&bytes));
    Ok(live_segment_response(&bytes, content_type))
}

fn live_segment_response(bytes: &[u8], content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // A live segment's bytes never change once written, so a client
            // that did re-request one may reuse it; the *playlist* is what
            // must never be cached.
            (header::CACHE_CONTROL, SEGMENT_CACHE),
        ],
        bytes.to_vec(),
    )
        .into_response()
}

/// Fetches one segment's byte range from the origin into the store, coalescing
/// concurrent first-plays of the same segment. Returns the cache file name.
async fn fetch_and_cache_segment(
    state: &AppState,
    media_id: i64,
    origin: &str,
    start: u64,
    len: u64,
) -> Result<String, ApiError> {
    if hls_disabled(state).await? {
        return Err(ApiError::NotFound);
    }
    let start_i = i64::try_from(start).map_err(|_| ApiError::NotFound)?;
    let len_i = i64::try_from(len).map_err(|_| ApiError::NotFound)?;
    let key = format!("{origin}#{start}-{len}");

    // Single-flight: if another request is already fetching this exact segment,
    // wait briefly for it to publish rather than hit the origin a second time.
    let guard = SegGuard::begin(&key);
    if guard.is_none() {
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(hit) =
                media::hls_segment_lookup(&state.pool, origin, start_i, len_i).await?
            {
                return Ok(hit.cache_file);
            }
            if !SegGuard::running(&key) {
                break; // the winner finished (or died) without publishing
            }
        }
    }
    // Re-check: the winner may have just published while we waited or raced.
    if let Some(hit) = media::hls_segment_lookup(&state.pool, origin, start_i, len_i).await? {
        return Ok(hit.cache_file);
    }

    // Scratch under the media root (same filesystem → the store moves it in by
    // rename); the range streams to disk, constant memory.
    let scratch_root = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&scratch_root)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let tmp = tempfile::Builder::new()
        .prefix("hls-")
        .tempfile_in(&scratch_root)
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let tmp_path = tmp.path().to_path_buf();
    let written = state
        .federation
        .fetch_range_to_file(origin, &tmp_path, start, len)
        .await
        .map_err(|error| {
            tracing::warn!(%error, origin, start, len, "HLS segment origin fetch failed");
            ApiError::NotFound
        })?;

    // Content-addressed name: two racers computing it get the same file, so a
    // duplicate put is idempotent. Alphanumeric+dot, so `/media/{file}` rules
    // would accept it too.
    let cache_file = format!("hls{}.mp4", segment_hash(origin, start, len));
    state
        .media
        .put_file(&cache_file, &tmp_path)
        .await
        .map_err(|e| ApiError::Internal(Box::new(e)))?;
    let bytes = i64::try_from(written).unwrap_or(i64::MAX);
    let recorded = media::hls_segment_record(
        &state.pool,
        media_id,
        origin,
        start_i,
        len_i,
        &cache_file,
        bytes,
    )
    .await?;
    drop(guard);
    if !recorded {
        // Lost the record race — the winner's name is identical (content-
        // addressed), so either file serves the same bytes; return the row's.
        if let Some(hit) = media::hls_segment_lookup(&state.pool, origin, start_i, len_i).await? {
            return Ok(hit.cache_file);
        }
    }
    Ok(cache_file)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn hls_disabled(state: &AppState) -> Result<bool, ApiError> {
    Ok(state
        .settings_cache
        .get(&state.pool)
        .await?
        .remote_video_max_mb
        <= 0)
}

/// What this proxy needs to know about an attachment before serving any part
/// of its stream: where the origin playlist tree is rooted (the SSRF fence),
/// and whether it is a live broadcast (which changes the caching rules for
/// every layer below).
struct Lane {
    master_url: String,
    live: bool,
    /// A live that is running right now. A broadcast that has not started, or
    /// that is over, has no playable playlist even when it still advertises a
    /// master URL.
    on_air: bool,
}

async fn lane(state: &AppState, media_id: i64) -> Result<Lane, ApiError> {
    if media::owner_suspended(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let item = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    Ok(Lane {
        master_url: item.hls_master_url.ok_or(ApiError::NotFound)?,
        live: item.live_state.is_some(),
        on_air: item.live_state.as_deref() == Some("live"),
    })
}

/// Re-reads the lane after a live-state refresh may have moved it.
async fn lane_reload(state: &AppState, media_id: i64) -> Result<Lane, ApiError> {
    lane(state, media_id).await
}

/// The directory URL of a playlist — its master's siblings (media playlists,
/// segments, init) all live here. The SSRF fence for proxied child URLs.
fn dir_prefix(url: &str) -> String {
    match url.rfind('/') {
        Some(i) => url[..=i].to_owned(),
        None => url.to_owned(),
    }
}

/// Decodes a proxied child URL and enforces it stays under the video's own HLS
/// directory on the origin — a client can only ask for files beneath the master
/// playlist's directory, never an arbitrary URL (SSRF/open-proxy fence).
fn decode_child(b64: &str, prefix: &str) -> Option<String> {
    let bytes = B64.decode(b64).ok()?;
    let url = String::from_utf8(bytes).ok()?;
    (plamenu_federation::is_federation_url(&url) && url.starts_with(prefix)).then_some(url)
}

fn pl_url(media_id: i64, abs: &str) -> String {
    format!("/media/hls/{media_id}/pl?u={}", B64.encode(abs.as_bytes()))
}

/// The proxied URL of a live playlist or segment.
///
/// Live children get their origin's file extension back on the end of the
/// *path*, where the VOD lane puts the origin in a query parameter. That is
/// not cosmetic: `FFmpeg`'s HLS demuxer — which the progressive gateway feeds
/// from these very URLs — refuses to open a segment whose URL does not end in
/// a recognised extension, and it is not alone in inferring the container from
/// the path. Base64url never produces a `.` or a `/`, so the last dot is
/// unambiguously ours.
fn live_child_url(media_id: i64, abs: &str) -> String {
    format!(
        "/media/hls/{media_id}/live/{}{}",
        B64.encode(abs.as_bytes()),
        origin_extension(abs)
    )
}

/// The origin file's extension, as a suffix to append (`.ts`, `.m3u8`, …).
/// Anything unrecognised is presented as MPEG-TS, which is what `PeerTube`
/// live segments are.
fn origin_extension(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some(ext) if ext.eq_ignore_ascii_case("m3u8") => ".m3u8",
        Some(ext) if ext.eq_ignore_ascii_case("mp4") => ".mp4",
        Some(ext) if ext.eq_ignore_ascii_case("m4s") => ".m4s",
        Some(ext) if ext.eq_ignore_ascii_case("aac") => ".aac",
        Some(ext) if ext.eq_ignore_ascii_case("vtt") => ".vtt",
        _ => ".ts",
    }
}

fn seg_url(media_id: i64, abs: &str, range: Option<(u64, u64)>) -> String {
    let u = B64.encode(abs.as_bytes());
    match range {
        Some((s, l)) => format!("/media/hls/{media_id}/seg?u={u}&s={s}&l={l}"),
        None => format!("/media/hls/{media_id}/seg?u={u}"),
    }
}

/// Resolves an HLS URI line against the playlist's own URL: absolute URLs pass
/// through, bare filenames join the playlist's directory (`PeerTube`'s own rule).
fn resolve(base: Option<&url::Url>, uri: &str) -> Option<String> {
    if uri.starts_with("https://") || uri.starts_with("http://") {
        Some(uri.to_owned())
    } else {
        base?.join(uri).ok().map(String::from)
    }
}

/// Parses `LEN[@OFFSET]` (EXT-X-BYTERANGE); an omitted offset continues from
/// `default_start` (the byte after the previous sub-range). Returns (start, len).
fn parse_byterange(spec: &str, default_start: u64) -> (u64, u64) {
    let spec = spec.trim();
    match spec.split_once('@') {
        Some((len, off)) => (
            off.trim().parse().unwrap_or(default_start),
            len.trim().parse().unwrap_or(0),
        ),
        None => (default_start, spec.parse().unwrap_or(0)),
    }
}

/// One attribute value from an `#EXT-X-…` attribute list (quote-aware split, so
/// a comma inside `CODECS="a,b"` doesn't break parsing). Strips surrounding
/// quotes.
fn attr(attrs: &str, name: &str) -> Option<String> {
    split_attrs(attrs).into_iter().find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// Whether an HLS URI names a (nested) playlist rather than a media segment.
fn is_m3u8(url: &str) -> bool {
    url.rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
}

/// Splits an HLS attribute list on commas that are not inside a quoted value.
fn split_attrs(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// A deterministic content-addressed cache name for a segment (fixed-seed
/// `DefaultHasher`, so concurrent racers agree on the file name).
fn segment_hash(origin: &str, start: u64, len: u64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    origin.hash(&mut hasher);
    start.hash(&mut hasher);
    len.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "https://peer.example/static/streaming-playlists/hls/vid/master.m3u8";

    #[test]
    fn rewrites_master_variants_and_audio_group() {
        let body = "#EXTM3U\n#EXT-X-VERSION:7\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio\",DEFAULT=YES,URI=\"vid-0-fragmented.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"audio\"\n\
            vid-720-fragmented.m3u8\n";
        let out = rewrite_playlist(body, MASTER, 42, false);
        // Stream + audio URIs are proxied; the stream-inf attributes are verbatim.
        assert!(out.contains("#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"audio\""));
        assert!(
            out.contains("/media/hls/42/pl?u="),
            "variant playlist proxied: {out}"
        );
        assert!(
            out.contains("URI=\"/media/hls/42/pl?u="),
            "audio group playlist proxied: {out}"
        );
        // The proxied audio URI decodes back to the origin file, under prefix.
        let u = extract_first(&out, "pl?u=");
        assert_eq!(
            decode_child(&u, &dir_prefix(MASTER)).as_deref(),
            Some("https://peer.example/static/streaming-playlists/hls/vid/vid-0-fragmented.m3u8")
        );
    }

    #[test]
    fn folds_byterange_into_segment_urls() {
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n\
            #EXT-X-MAP:URI=\"vid-720-fragmented.mp4\",BYTERANGE=\"1376@0\"\n\
            #EXTINF:4.000000,\n\
            #EXT-X-BYTERANGE:19987@1376\n\
            vid-720-fragmented.mp4\n\
            #EXTINF:1.000000,\n\
            #EXT-X-BYTERANGE:9147@21363\n\
            vid-720-fragmented.mp4\n\
            #EXT-X-ENDLIST\n";
        let media_url =
            "https://peer.example/static/streaming-playlists/hls/vid/vid-720-fragmented.m3u8";
        let out = rewrite_playlist(body, media_url, 7, false);
        // The init segment keeps its byte range, folded into the URL, no BYTERANGE attr.
        assert!(
            out.contains("#EXT-X-MAP:URI=\"/media/hls/7/seg?u="),
            "map proxied: {out}"
        );
        assert!(
            out.contains("&s=0&l=1376\""),
            "init byte range in the url: {out}"
        );
        assert!(
            !out.contains("#EXT-X-BYTERANGE"),
            "byterange folded away: {out}"
        );
        // Each segment becomes a distinct proxied URL carrying its range.
        assert!(
            out.contains("/media/hls/7/seg?u=") && out.contains("&s=1376&l=19987"),
            "seg1: {out}"
        );
        assert!(out.contains("&s=21363&l=9147"), "seg2: {out}");
        // EXTINF and ENDLIST are verbatim.
        assert!(out.contains("#EXTINF:4.000000,") && out.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn ssrf_fence_rejects_out_of_prefix_urls() {
        let prefix = dir_prefix(MASTER);
        // A URL under the video's own HLS dir is allowed.
        let ok = B64
            .encode("https://peer.example/static/streaming-playlists/hls/vid/seg.mp4".as_bytes());
        assert!(decode_child(&ok, &prefix).is_some());
        // A sibling/other-path or other-host URL is refused.
        let evil = B64.encode("https://peer.example/etc/passwd".as_bytes());
        assert!(decode_child(&evil, &prefix).is_none());
        let other_host =
            B64.encode("https://evil.example/static/streaming-playlists/hls/vid/x.mp4".as_bytes());
        assert!(decode_child(&other_host, &prefix).is_none());
        // Non-base64 is refused.
        assert!(decode_child("!!!not base64!!!", &prefix).is_none());
    }

    fn extract_first(haystack: &str, needle: &str) -> String {
        let start = haystack.find(needle).unwrap() + needle.len();
        let rest = &haystack[start..];
        let end = rest.find(['"', '\n', '&']).unwrap_or(rest.len());
        rest[..end].to_owned()
    }
}
