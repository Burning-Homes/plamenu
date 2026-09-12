//! Stable sparse-range compatibility gateway for remote A/V.
//!
//! Conventional Mastodon clients only understand one `MediaAttachment.url`.
//! For a `PeerTube` rendition that already contains audio + video, that URL can
//! be a sparse, fixed-block Range proxy: it starts immediately, seeks like an
//! ordinary MP4, caches watched blocks, and owns every upstream request so a
//! disconnected viewer cannot leave a whole-file background download behind.
//! Podcast audio uses the same lane: the first origin chunk reaches the listener
//! while the aligned block is still being written to cache.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::future::join_all;
use futures_util::{StreamExt, stream};
use plamenu_db::media;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::virtual_mp4::{self, RelativePatch};
use crate::error::ApiError;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

const CACHE_CONTROL: &str = "public, max-age=60";
/// A disengaged viewer can cost at most the current block. Small blocks also
/// make arbitrary player seeks converge with HLS's watched-parts cache.
const BLOCK_BYTES: u64 = 512 * 1024;
const IO_CHUNK: usize = 64 * 1024;
const MAX_VIRTUAL_LAYOUTS: usize = 256;
const MAX_FRAGMENT_HEADERS: usize = 2_048;
const MAX_ORIGIN_TOTALS: usize = 4_096;

static BLOCK_INFLIGHT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static VIRTUAL_LAYOUTS: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<VirtualLayout>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));
static FRAGMENT_HEADERS: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<Vec<u8>>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));
static HEADER_INFLIGHT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
static ORIGIN_TOTALS: LazyLock<tokio::sync::Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

struct BlockGuard(String);

impl BlockGuard {
    fn begin(key: &str) -> Option<Self> {
        let inserted = BLOCK_INFLIGHT.lock_or_recover().insert(key.to_owned());
        inserted.then(|| Self(key.to_owned()))
    }

    fn running(key: &str) -> bool {
        BLOCK_INFLIGHT.lock_or_recover().contains(key)
    }
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        BLOCK_INFLIGHT.lock_or_recover().remove(&self.0);
    }
}

struct HeaderGuard(String);

impl HeaderGuard {
    fn begin(key: &str) -> Option<Self> {
        let inserted = HEADER_INFLIGHT.lock_or_recover().insert(key.to_owned());
        inserted.then(|| Self(key.to_owned()))
    }

    fn running(key: &str) -> bool {
        HEADER_INFLIGHT.lock_or_recover().contains(key)
    }
}

impl Drop for HeaderGuard {
    fn drop(&mut self) {
        HEADER_INFLIGHT.lock_or_recover().remove(&self.0);
    }
}

enum SourceBody {
    Local { file: String },
    Direct { origin: String },
    Virtual(Arc<VirtualLayout>),
}

struct Source {
    total: u64,
    content_type: String,
    body: SourceBody,
}

enum AudioSource {
    Stream(Source),
    Origin(String),
}

#[derive(Deserialize)]
pub struct AudioQuery {
    #[serde(default)]
    d: Option<String>,
}

impl AudioQuery {
    fn allow_direct(&self) -> bool {
        self.d.as_deref() == Some("1")
    }
}

#[derive(Clone)]
struct Patch {
    /// Absolute offset in the remote source file.
    offset: u64,
    expected: [u8; 4],
    replacement: [u8; 4],
}

struct VirtualLayout {
    total: u64,
    pieces: Vec<VirtualPiece>,
}

struct VirtualPiece {
    start: u64,
    len: u64,
    body: VirtualPieceBody,
}

enum VirtualPieceBody {
    Inline(Arc<Vec<u8>>),
    FragmentHeader(FragmentHeader),
    Remote {
        origin: String,
        origin_total: u64,
        origin_start: u64,
        patches: Arc<Vec<Patch>>,
    },
}

struct HeaderSource {
    origin: String,
    part: virtual_mp4::BytePart,
}

struct FragmentHeader {
    cache_key: String,
    video: HeaderSource,
    audio: Vec<HeaderSource>,
    old_audio_id: u32,
    new_audio_id: u32,
}

async fn source(state: &AppState, media_id: i64) -> Result<Source, ApiError> {
    if media::owner_suspended(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    // The route is deliberately limited to HLS-native rows. Plain remote
    // attachments retain the ordinary media proxy behavior.
    media::hls_master_url(&state.pool, media_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let item = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound)?;
    if item.processing == "complete"
        && let Some(file) = item.file_name
        && let Ok(opened) = state.media.open(&file).await
    {
        return Ok(Source {
            total: opened.len,
            content_type: "video/mp4".to_owned(),
            body: SourceBody::Local { file },
        });
    }
    // A companion means the chosen rendition is video-only. The virtual MP4
    // layer handles that case; never expose the silent origin file here.
    let origin = item.remote_url.ok_or(ApiError::NotFound)?;
    let renditions = media::renditions_for(&state.pool, &[media_id]).await?;
    let declared = renditions
        .get(&media_id)
        .and_then(|rows| rows.iter().find(|row| row.origin_url == origin))
        .and_then(|row| row.size_bytes)
        .and_then(|size| u64::try_from(size).ok())
        .filter(|size| *size > 0);
    let total = match declared {
        Some(total) => total,
        None => {
            // A one-byte Range probe obtains the total without starting a
            // body download. Dropping the returned stream cancels its body.
            state
                .federation
                .fetch_media_range(&origin, 0, 1)
                .await
                .map_err(|_| ApiError::NotFound)?
                .total_len
        }
    };
    if let Some(audio) = item.remote_audio_url {
        let audio_declared = renditions
            .get(&media_id)
            .and_then(|rows| rows.iter().find(|row| row.origin_url == audio))
            .and_then(|row| row.size_bytes)
            .and_then(|size| u64::try_from(size).ok())
            .filter(|size| *size > 0);
        let audio_total = match audio_declared {
            Some(total) => total,
            None => probe_total(state, &audio).await?,
        };
        let layout = virtual_layout(state, media_id, &origin, total, &audio, audio_total).await?;
        Ok(Source {
            total: layout.total,
            content_type: "video/mp4".to_owned(),
            body: SourceBody::Virtual(layout),
        })
    } else {
        // A muxed PeerTube fMP4 still has zero init durations and only local
        // four-second indexes. Prefer a metadata-only virtual layout with a
        // complete seek map; retain the old sparse proxy as a safe fallback
        // for non-PeerTube/malformed HLS publications.
        match muxed_virtual_layout(state, media_id, &origin, total).await {
            Ok(layout) => Ok(Source {
                total: layout.total,
                content_type: "video/mp4".to_owned(),
                body: SourceBody::Virtual(layout),
            }),
            Err(_) => Ok(Source {
                total,
                content_type: "video/mp4".to_owned(),
                body: SourceBody::Direct { origin },
            }),
        }
    }
}

/// Resolves an ordinary remote audio attachment into the same bounded sparse
/// cache used by progressive video. The complete origin size is known before
/// any body bytes are exposed, so the operator's A/V limit is still a hard
/// ceiling. When caching is disabled, the file is oversized, or policy rejects
/// its domain, only an explicitly opted-in viewer may be redirected upstream.
async fn audio_source(
    state: &AppState,
    media_id: i64,
    allow_direct: bool,
) -> Result<AudioSource, ApiError> {
    if media::owner_suspended(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let item = media::find_by_ids(&state.pool, &[media_id])
        .await?
        .into_iter()
        .next()
        .filter(|item| item.kind_or_derived() == "audio" && item.download_on_demand)
        .ok_or(ApiError::NotFound)?;

    let Some(origin) = item.remote_url else {
        let file = item.file_name.ok_or(ApiError::NotFound)?;
        let opened = state
            .media
            .open(&file)
            .await
            .map_err(|_| ApiError::NotFound)?;
        return Ok(AudioSource::Stream(Source {
            total: opened.len,
            content_type: item.content_type,
            body: SourceBody::Local { file },
        }));
    };
    let fallback = || {
        allow_direct
            .then(|| AudioSource::Origin(origin.clone()))
            .ok_or(ApiError::NotFound)
    };
    if plamenu_db::instance_policy::account_domain_rejects_media(&state.pool, item.account_id)
        .await?
    {
        return fallback();
    }
    let settings = state.settings_cache.get(&state.pool).await?;
    let budget = u64::try_from(settings.remote_video_max_mb)
        .unwrap_or(0)
        .saturating_mul(1024 * 1024);
    if budget == 0 {
        return fallback();
    }
    let total = match cached_origin_total(state, &origin).await {
        Ok(total) if total > 0 && total <= budget => total,
        Ok(_) | Err(_) => return fallback(),
    };
    Ok(AudioSource::Stream(Source {
        total,
        content_type: item.content_type,
        body: SourceBody::Direct { origin },
    }))
}

async fn cached_origin_total(state: &AppState, origin: &str) -> Result<u64, ApiError> {
    if let Some(total) = ORIGIN_TOTALS.lock().await.get(origin).copied() {
        return Ok(total);
    }
    let total = probe_total(state, origin).await?;
    let mut totals = ORIGIN_TOTALS.lock().await;
    if totals.len() >= MAX_ORIGIN_TOTALS
        && let Some(evicted) = totals.keys().next().cloned()
    {
        totals.remove(&evicted);
    }
    totals.insert(origin.to_owned(), total);
    Ok(total)
}

async fn probe_total(state: &AppState, origin: &str) -> Result<u64, ApiError> {
    Ok(state
        .federation
        .fetch_media_range(origin, 0, 1)
        .await
        .map_err(|_| ApiError::NotFound)?
        .total_len)
}

fn hls_media_playlist_url(origin: &str) -> Option<String> {
    let mut url = url::Url::parse(origin).ok()?;
    let playlist_path = url
        .path()
        .strip_suffix("-fragmented.mp4")
        .map(|stem| format!("{stem}.m3u8"))?;
    url.set_path(&playlist_path);
    Some(url.into())
}

async fn muxed_virtual_layout(
    state: &AppState,
    media_id: i64,
    origin: &str,
    origin_total: u64,
) -> Result<Arc<VirtualLayout>, ApiError> {
    let cache_key = format!("muxed:{media_id}:{origin}:{origin_total}");
    if let Some(cached) = VIRTUAL_LAYOUTS.lock().await.get(&cache_key).cloned() {
        return Ok(cached);
    }
    let playlist_url = hls_media_playlist_url(origin).ok_or(ApiError::NotFound)?;
    let playlist = state
        .federation
        .fetch_media(&playlist_url)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let playlist = virtual_mp4::parse_media_playlist(
        std::str::from_utf8(&playlist.bytes).map_err(|_| ApiError::NotFound)?,
    )
    .map_err(|_| ApiError::NotFound)?;
    if playlist
        .init
        .start
        .checked_add(playlist.init.len)
        .is_none_or(|end| end > origin_total)
        || playlist.segments.iter().any(|segment| {
            segment
                .bytes
                .start
                .checked_add(segment.bytes.len)
                .is_none_or(|end| end > origin_total)
        })
    {
        return Err(ApiError::NotFound);
    }
    let init = collect_range(state, origin, playlist.init.start, playlist.init.len).await?;
    let (patched_init, reference_id, timescale) =
        virtual_mp4::prepare_muxed_init(&init, playlist.duration())
            .map_err(|_| ApiError::NotFound)?;
    let first = playlist.segments.first().ok_or(ApiError::NotFound)?;
    let first_prefix = collect_range(
        state,
        origin,
        first.bytes.start,
        first.bytes.len.min(virtual_mp4::FRAGMENT_HEADER_BYTES),
    )
    .await?;
    let local_patches =
        virtual_mp4::fragment_sidx_patches(&first_prefix).map_err(|_| ApiError::NotFound)?;
    let references = playlist
        .segments
        .iter()
        .map(|segment| virtual_mp4::SegmentReference {
            size: segment.bytes.len,
            duration: segment.duration,
        })
        .collect::<Vec<_>>();
    let index = virtual_mp4::global_sidx(reference_id, timescale, &references)
        .map_err(|_| ApiError::NotFound)?;
    let mut prefix = patched_init;
    prefix.extend_from_slice(&index);
    let prefix = Arc::new(prefix);
    let mut cursor = prefix.len() as u64;
    let mut pieces = Vec::with_capacity(1 + playlist.segments.len());
    pieces.push(VirtualPiece {
        start: 0,
        len: cursor,
        body: VirtualPieceBody::Inline(prefix),
    });
    for segment in playlist.segments {
        let patches = local_patches
            .iter()
            .map(|patch: &RelativePatch| Patch {
                offset: segment.bytes.start + patch.offset,
                expected: patch.expected,
                replacement: patch.replacement,
            })
            .collect();
        pieces.push(VirtualPiece {
            start: cursor,
            len: segment.bytes.len,
            body: VirtualPieceBody::Remote {
                origin: origin.to_owned(),
                origin_total,
                origin_start: segment.bytes.start,
                patches: Arc::new(patches),
            },
        });
        cursor = cursor
            .checked_add(segment.bytes.len)
            .ok_or(ApiError::NotFound)?;
    }
    let layout = Arc::new(VirtualLayout {
        total: cursor,
        pieces,
    });
    let mut layouts = VIRTUAL_LAYOUTS.lock().await;
    if layouts.len() >= MAX_VIRTUAL_LAYOUTS
        && let Some(oldest) = layouts.keys().next().cloned()
    {
        layouts.remove(&oldest);
    }
    layouts.insert(cache_key, Arc::clone(&layout));
    Ok(layout)
}

async fn collect_range(
    state: &AppState,
    origin: &str,
    start: u64,
    len: u64,
) -> Result<Vec<u8>, ApiError> {
    let mut fetched = state
        .federation
        .fetch_media_range(origin, start, len)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let mut result = Vec::with_capacity(
        usize::try_from(len).map_err(|error| ApiError::Internal(Box::new(error)))?,
    );
    while let Some(chunk) = fetched.bytes.next().await {
        result.extend_from_slice(&chunk.map_err(|_| ApiError::NotFound)?);
    }
    if result.len() as u64 != len {
        return Err(ApiError::NotFound);
    }
    Ok(result)
}

#[allow(clippy::too_many_lines, reason = "one linear virtual-file layout pass")]
async fn virtual_layout(
    state: &AppState,
    media_id: i64,
    video_origin: &str,
    video_total: u64,
    audio_origin: &str,
    audio_total: u64,
) -> Result<Arc<VirtualLayout>, ApiError> {
    let cache_key = format!("{media_id}:{video_origin}:{video_total}:{audio_origin}:{audio_total}");
    if let Some(cached) = VIRTUAL_LAYOUTS.lock().await.get(&cache_key).cloned() {
        return Ok(cached);
    }
    let video_playlist_url = hls_media_playlist_url(video_origin).ok_or(ApiError::NotFound)?;
    let audio_playlist_url = hls_media_playlist_url(audio_origin).ok_or(ApiError::NotFound)?;
    let (video_playlist, audio_playlist) = tokio::join!(
        state.federation.fetch_media(&video_playlist_url),
        state.federation.fetch_media(&audio_playlist_url),
    );
    let video_playlist = video_playlist.map_err(|_| ApiError::NotFound)?;
    let audio_playlist = audio_playlist.map_err(|_| ApiError::NotFound)?;
    let video_playlist = virtual_mp4::parse_media_playlist(
        std::str::from_utf8(&video_playlist.bytes).map_err(|_| ApiError::NotFound)?,
    )
    .map_err(|_| ApiError::NotFound)?;
    let audio_playlist = virtual_mp4::parse_media_playlist(
        std::str::from_utf8(&audio_playlist.bytes).map_err(|_| ApiError::NotFound)?,
    )
    .map_err(|_| ApiError::NotFound)?;
    if video_playlist
        .init
        .start
        .checked_add(video_playlist.init.len)
        .is_none_or(|end| end > video_total)
        || audio_playlist
            .init
            .start
            .checked_add(audio_playlist.init.len)
            .is_none_or(|end| end > audio_total)
    {
        return Err(ApiError::NotFound);
    }
    let (video_init, audio_init) = tokio::join!(
        collect_range(
            state,
            video_origin,
            video_playlist.init.start,
            video_playlist.init.len
        ),
        collect_range(
            state,
            audio_origin,
            audio_playlist.init.start,
            audio_playlist.init.len
        ),
    );
    let (combined_init, video_id, video_timescale, old_audio_id, new_audio_id) =
        virtual_mp4::combine_init(
            &video_init?,
            &audio_init?,
            video_playlist.duration(),
            audio_playlist.duration(),
        )
        .map_err(|_| ApiError::NotFound)?;
    let groups = virtual_mp4::segment_groups(&video_playlist, &audio_playlist);
    let mut references = Vec::with_capacity(groups.len());
    for group in &groups {
        let mut size = video_playlist.segments[group.video].bytes.len;
        for audio in &audio_playlist.segments[group.audio.clone()] {
            size = size
                .checked_add(audio.bytes.len)
                .ok_or(ApiError::NotFound)?;
        }
        references.push(virtual_mp4::SegmentReference {
            size: size
                .checked_add(virtual_mp4::FRAGMENT_HEADER_BYTES)
                .ok_or(ApiError::NotFound)?,
            duration: group.duration,
        });
    }
    let global_index = virtual_mp4::global_sidx(video_id, video_timescale, &references)
        .map_err(|_| ApiError::NotFound)?;
    let mut prefix = combined_init;
    prefix.extend_from_slice(&global_index);

    let mut pieces = Vec::with_capacity(
        1 + groups.len() + video_playlist.segments.len() + audio_playlist.segments.len(),
    );
    let init = Arc::new(prefix);
    let mut cursor = init.len() as u64;
    pieces.push(VirtualPiece {
        start: 0,
        len: cursor,
        body: VirtualPieceBody::Inline(init),
    });
    for (group_index, group) in groups.into_iter().enumerate() {
        let video = &video_playlist.segments[group.video];
        pieces.push(VirtualPiece {
            start: cursor,
            len: virtual_mp4::FRAGMENT_HEADER_BYTES,
            body: VirtualPieceBody::FragmentHeader(FragmentHeader {
                cache_key: format!("{cache_key}:{group_index}"),
                video: HeaderSource {
                    origin: video_origin.to_owned(),
                    part: video.bytes,
                },
                audio: audio_playlist.segments[group.audio.clone()]
                    .iter()
                    .map(|audio| HeaderSource {
                        origin: audio_origin.to_owned(),
                        part: audio.bytes,
                    })
                    .collect(),
                old_audio_id,
                new_audio_id,
            }),
        });
        cursor = cursor
            .checked_add(virtual_mp4::FRAGMENT_HEADER_BYTES)
            .ok_or(ApiError::NotFound)?;
        for (segment, origin, origin_total) in std::iter::once((video, video_origin, video_total))
            .chain(
                audio_playlist.segments[group.audio]
                    .iter()
                    .map(|audio| (audio, audio_origin, audio_total)),
            )
        {
            let part = segment.bytes;
            if part
                .start
                .checked_add(part.len)
                .is_none_or(|end| end > origin_total)
            {
                return Err(ApiError::NotFound);
            }
            pieces.push(VirtualPiece {
                start: cursor,
                len: part.len,
                body: VirtualPieceBody::Remote {
                    origin: origin.to_owned(),
                    origin_total,
                    origin_start: part.start,
                    patches: Arc::new(Vec::new()),
                },
            });
            cursor = cursor.checked_add(part.len).ok_or(ApiError::NotFound)?;
        }
    }
    let layout = Arc::new(VirtualLayout {
        total: cursor,
        pieces,
    });
    let mut layouts = VIRTUAL_LAYOUTS.lock().await;
    if layouts.len() >= MAX_VIRTUAL_LAYOUTS
        && let Some(oldest) = layouts.keys().next().cloned()
    {
        layouts.remove(&oldest);
    }
    layouts.insert(cache_key, Arc::clone(&layout));
    Ok(layout)
}

enum RequestedRange {
    Full,
    Partial(u64, u64),
    Unsatisfiable,
}

fn requested_range(headers: &HeaderMap, total: u64) -> RequestedRange {
    let Some(raw) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return RequestedRange::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RequestedRange::Full;
    };
    if spec.contains(',') {
        return RequestedRange::Full;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return RequestedRange::Full;
    };
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let Ok(suffix) = suffix.parse::<u64>() else {
                return RequestedRange::Full;
            };
            if suffix == 0 || total == 0 {
                return RequestedRange::Unsatisfiable;
            }
            (total.saturating_sub(suffix), total - 1)
        }
        (start, "") => {
            let Ok(start) = start.parse::<u64>() else {
                return RequestedRange::Full;
            };
            if start >= total {
                return RequestedRange::Unsatisfiable;
            }
            (start, total - 1)
        }
        (start, end) => {
            let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                return RequestedRange::Full;
            };
            if start > end || start >= total {
                return RequestedRange::Unsatisfiable;
            }
            (start, end.min(total - 1))
        }
    };
    RequestedRange::Partial(start, end)
}

pub async fn head(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
) -> Result<Response, ApiError> {
    let source = source(&state, media_id).await?;
    Ok(head_response(&source))
}

fn head_response(source: &Source) -> Response {
    (
        [
            (header::CONTENT_TYPE, source.content_type.clone()),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_owned()),
            (header::ACCEPT_RANGES, "bytes".to_owned()),
            (header::CONTENT_LENGTH, source.total.to_string()),
        ],
        Body::empty(),
    )
        .into_response()
}

pub async fn get(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let source = source(&state, media_id).await?;
    serve_source(state, media_id, headers, source).await
}

pub async fn audio_head(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(query): Query<AudioQuery>,
) -> Result<Response, ApiError> {
    match audio_source(&state, media_id, query.allow_direct()).await? {
        AudioSource::Stream(source) => Ok(head_response(&source)),
        AudioSource::Origin(origin) => Ok(super::media::redirect_origin(&origin)),
    }
}

pub async fn audio_get(
    State(state): State<AppState>,
    Path(media_id): Path<i64>,
    Query(query): Query<AudioQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    match audio_source(&state, media_id, query.allow_direct()).await? {
        AudioSource::Stream(source) => serve_source(state, media_id, headers, source).await,
        AudioSource::Origin(origin) => Ok(super::media::redirect_origin(&origin)),
    }
}

async fn serve_source(
    state: AppState,
    media_id: i64,
    headers: HeaderMap,
    source: Source,
) -> Result<Response, ApiError> {
    let (status, start, end) = match requested_range(&headers, source.total) {
        RequestedRange::Full => (StatusCode::OK, 0, source.total.saturating_sub(1)),
        RequestedRange::Partial(start, end) => (StatusCode::PARTIAL_CONTENT, start, end),
        RequestedRange::Unsatisfiable => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", source.total))],
            )
                .into_response());
        }
    };
    if source.total == 0 {
        return Err(ApiError::NotFound);
    }
    let len = end - start + 1;
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, std::io::Error>>(8);
    match source.body {
        SourceBody::Local { file } => {
            media::touch_served(&state.pool, media_id).await.ok();
            tokio::spawn(async move {
                if let Err(error) = stream_cached(&state, &file, 0, start, end, &[], &tx).await {
                    let _ = tx.send(Err(error)).await;
                }
            });
        }
        SourceBody::Direct { origin } => {
            tokio::spawn(stream_blocks(
                state,
                media_id,
                origin,
                source.total,
                start,
                end,
                Arc::new(Vec::new()),
                tx,
            ));
        }
        SourceBody::Virtual(layout) => {
            tokio::spawn(stream_virtual(state, media_id, layout, start, end, tx));
        }
    }
    let body_stream = stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut response = (
        status,
        [
            (header::CONTENT_TYPE, source.content_type),
            (header::CACHE_CONTROL, CACHE_CONTROL.to_owned()),
            (header::ACCEPT_RANGES, "bytes".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(body_stream),
    )
        .into_response();
    if status == StatusCode::PARTIAL_CONTENT {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", source.total)
                .parse()
                .map_err(|error| ApiError::Internal(Box::new(error)))?,
        );
    }
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
async fn stream_blocks(
    state: AppState,
    media_id: i64,
    origin: String,
    total: u64,
    request_start: u64,
    request_end: u64,
    patches: Arc<Vec<Patch>>,
    tx: mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
) -> bool {
    let first = request_start / BLOCK_BYTES;
    let last = request_end / BLOCK_BYTES;
    for block in first..=last {
        if tx.is_closed() {
            return false;
        }
        let block_start = block * BLOCK_BYTES;
        let block_len = BLOCK_BYTES.min(total - block_start);
        let send_start = request_start.max(block_start);
        let send_end = request_end.min(block_start + block_len - 1);
        if let Err(error) = stream_one_block(
            &state,
            media_id,
            &origin,
            block_start,
            block_len,
            send_start,
            send_end,
            &patches,
            &tx,
        )
        .await
        {
            let _ = tx.send(Err(error)).await;
            return false;
        }
    }
    true
}

async fn stream_virtual(
    state: AppState,
    media_id: i64,
    layout: Arc<VirtualLayout>,
    request_start: u64,
    request_end: u64,
    tx: mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
) {
    for piece in &layout.pieces {
        let piece_end = piece.start + piece.len - 1;
        let overlap_start = request_start.max(piece.start);
        let overlap_end = request_end.min(piece_end);
        if overlap_start > overlap_end {
            continue;
        }
        if tx.is_closed() {
            return;
        }
        match &piece.body {
            VirtualPieceBody::Inline(bytes) => {
                let from = usize::try_from(overlap_start - piece.start).unwrap_or(0);
                let to = usize::try_from(overlap_end - piece.start + 1).unwrap_or(bytes.len());
                if tx.send(Ok(bytes[from..to].to_vec())).await.is_err() {
                    return;
                }
            }
            VirtualPieceBody::FragmentHeader(spec) => {
                let header = match cached_fragment_header(&state, piece.start, spec).await {
                    Ok(header) => header,
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                };
                let from = usize::try_from(overlap_start - piece.start).unwrap_or(0);
                let to = usize::try_from(overlap_end - piece.start + 1).unwrap_or(header.len());
                if tx.send(Ok(header[from..to].to_vec())).await.is_err() {
                    return;
                }
            }
            VirtualPieceBody::Remote {
                origin,
                origin_total,
                origin_start,
                patches,
            } => {
                let source_start = origin_start
                    .checked_add(overlap_start - piece.start)
                    .unwrap_or(*origin_total);
                let source_end = origin_start
                    .checked_add(overlap_end - piece.start)
                    .unwrap_or(*origin_total);
                if !stream_blocks(
                    state.clone(),
                    media_id,
                    origin.clone(),
                    *origin_total,
                    source_start,
                    source_end,
                    Arc::clone(patches),
                    tx.clone(),
                )
                .await
                {
                    return;
                }
                if tx.is_closed() {
                    return;
                }
            }
        }
    }
}

async fn cached_fragment_header(
    state: &AppState,
    header_start: u64,
    spec: &FragmentHeader,
) -> std::io::Result<Arc<Vec<u8>>> {
    if let Some(hit) = FRAGMENT_HEADERS.lock().await.get(&spec.cache_key).cloned() {
        return Ok(hit);
    }
    let _guard = loop {
        if let Some(guard) = HeaderGuard::begin(&spec.cache_key) {
            break guard;
        }
        while HeaderGuard::running(&spec.cache_key) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if let Some(hit) = FRAGMENT_HEADERS.lock().await.get(&spec.cache_key).cloned() {
                return Ok(hit);
            }
        }
    };
    let header = Arc::new(build_fragment_header(state, header_start, spec).await?);
    let mut headers = FRAGMENT_HEADERS.lock().await;
    if headers.len() >= MAX_FRAGMENT_HEADERS
        && let Some(evicted) = headers.keys().next().cloned()
    {
        headers.remove(&evicted);
    }
    headers.insert(spec.cache_key.clone(), Arc::clone(&header));
    Ok(header)
}

async fn build_fragment_header(
    state: &AppState,
    header_start: u64,
    spec: &FragmentHeader,
) -> std::io::Result<Vec<u8>> {
    let probe_len = spec.video.part.len.min(virtual_mp4::FRAGMENT_HEADER_BYTES);
    let video = collect_range(state, &spec.video.origin, spec.video.part.start, probe_len);
    let mut audio_lens = Vec::with_capacity(spec.audio.len());
    let mut audio_fetches = Vec::with_capacity(spec.audio.len());
    for source in &spec.audio {
        if source.part.len == 0 {
            return Err(std::io::Error::other("empty audio fragment"));
        }
        audio_lens.push(source.part.len);
        audio_fetches.push(collect_range(
            state,
            &source.origin,
            source.part.start,
            source.part.len.min(virtual_mp4::FRAGMENT_HEADER_BYTES),
        ));
    }
    let (video, audio) = tokio::join!(video, join_all(audio_fetches));
    let video = video.map_err(std::io::Error::other)?;
    let audio = audio
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    virtual_mp4::combined_fragment_header(
        &video,
        spec.video.part.len,
        &audio,
        &audio_lens,
        spec.old_audio_id,
        spec.new_audio_id,
        header_start,
    )
    .map_err(std::io::Error::other)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn stream_one_block(
    state: &AppState,
    media_id: i64,
    origin: &str,
    block_start: u64,
    block_len: u64,
    send_start: u64,
    send_end: u64,
    patches: &[Patch],
    tx: &mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
) -> std::io::Result<()> {
    let start_i = i64::try_from(block_start).map_err(std::io::Error::other)?;
    let len_i = i64::try_from(block_len).map_err(std::io::Error::other)?;
    if let Some(hit) = media::hls_segment_lookup(&state.pool, origin, start_i, len_i)
        .await
        .map_err(std::io::Error::other)?
    {
        match stream_cached(
            state,
            &hit.cache_file,
            block_start,
            send_start,
            send_end,
            patches,
            tx,
        )
        .await
        {
            // The row outlived its file (lost disk, out-of-band deletion):
            // forget it and refetch the block below. `open` is the first
            // thing `stream_cached` does, so nothing was sent yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    origin,
                    block_start,
                    cache_file = %hit.cache_file,
                    "cached video block file missing; forgetting row and refetching"
                );
                media::hls_segment_forget(&state.pool, origin, start_i, len_i)
                    .await
                    .map_err(std::io::Error::other)?;
            }
            served => {
                media::hls_segment_touch(&state.pool, origin, start_i, len_i)
                    .await
                    .ok();
                return served;
            }
        }
    }

    let key = format!("{origin}#{block_start}-{block_len}");
    let guard = loop {
        if let Some(guard) = BlockGuard::begin(&key) {
            break guard;
        }
        while BlockGuard::running(&key) {
            if tx.is_closed() {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if let Some(hit) = media::hls_segment_lookup(&state.pool, origin, start_i, len_i)
                .await
                .map_err(std::io::Error::other)?
            {
                return stream_cached(
                    state,
                    &hit.cache_file,
                    block_start,
                    send_start,
                    send_end,
                    patches,
                    tx,
                )
                .await;
            }
        }
        // The first fetch may have failed before publishing. Race all waiting
        // viewers back through `begin`; exactly one retries while the rest
        // continue waiting instead of receiving a spurious contention error.
    };
    let _guard = guard;

    let scratch_root = state.config.media_dir.join("tmp");
    tokio::fs::create_dir_all(&scratch_root).await?;
    let tmp = tempfile::Builder::new()
        .prefix("progressive-")
        .tempfile_in(&scratch_root)?;
    let tmp_path = tmp.path().to_path_buf();
    let mut output = tokio::fs::File::create(&tmp_path).await?;
    let mut fetched = state
        .federation
        .fetch_media_range(origin, block_start, block_len)
        .await
        .map_err(std::io::Error::other)?;
    if fetched.total_len < block_start + block_len {
        return Err(std::io::Error::other(
            "origin range is shorter than declared media",
        ));
    }
    let mut offset = block_start;
    let mut written = 0_u64;
    while let Some(chunk) = fetched.bytes.next().await {
        let chunk = chunk.map_err(std::io::Error::other)?;
        output.write_all(&chunk).await?;
        let chunk_start = offset;
        let chunk_end = offset + chunk.len() as u64;
        let overlap_start = chunk_start.max(send_start);
        let overlap_end = chunk_end.min(send_end + 1);
        if overlap_start < overlap_end {
            let from =
                usize::try_from(overlap_start - chunk_start).map_err(std::io::Error::other)?;
            let to = usize::try_from(overlap_end - chunk_start).map_err(std::io::Error::other)?;
            let mut outgoing = chunk[from..to].to_vec();
            apply_patches(&mut outgoing, overlap_start, patches)?;
            if tx.send(Ok(outgoing)).await.is_err() {
                // Dropping `fetched` here cancels the upstream response.
                return Ok(());
            }
        }
        offset = chunk_end;
        written += chunk.len() as u64;
    }
    if written != block_len {
        return Err(std::io::Error::other(format!(
            "origin returned {written} bytes for a {block_len}-byte range"
        )));
    }
    output.flush().await?;
    drop(output);
    let cache_file = format!("prog{}.mp4", block_hash(origin, block_start, block_len));
    state.media.put_file(&cache_file, &tmp_path).await?;
    media::hls_segment_record(
        &state.pool,
        media_id,
        origin,
        start_i,
        len_i,
        &cache_file,
        len_i,
    )
    .await
    .map_err(std::io::Error::other)?;
    media::hls_segment_touch(&state.pool, origin, start_i, len_i)
        .await
        .ok();
    Ok(())
}

async fn stream_cached(
    state: &AppState,
    cache_file: &str,
    block_start: u64,
    send_start: u64,
    send_end: u64,
    patches: &[Patch],
    tx: &mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
) -> std::io::Result<()> {
    let mut opened = state.media.open(cache_file).await?;
    let relative = send_start - block_start;
    opened
        .reader
        .seek(std::io::SeekFrom::Start(relative))
        .await?;
    let mut remaining = send_end - send_start + 1;
    let mut chunk_start = send_start;
    let mut buffer = vec![0_u8; IO_CHUNK];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(IO_CHUNK as u64)).unwrap_or(IO_CHUNK);
        let read = opened.reader.read(&mut buffer[..wanted]).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "cached progressive block is truncated",
            ));
        }
        let mut outgoing = buffer[..read].to_vec();
        apply_patches(&mut outgoing, chunk_start, patches)?;
        if tx.send(Ok(outgoing)).await.is_err() {
            return Ok(());
        }
        remaining -= read as u64;
        chunk_start += read as u64;
    }
    Ok(())
}

fn apply_patches(bytes: &mut [u8], absolute_start: u64, patches: &[Patch]) -> std::io::Result<()> {
    let absolute_end = absolute_start
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| std::io::Error::other("progressive patch range overflow"))?;
    for patch in patches {
        let patch_end = patch
            .offset
            .checked_add(4)
            .ok_or_else(|| std::io::Error::other("progressive patch offset overflow"))?;
        let overlap_start = absolute_start.max(patch.offset);
        let overlap_end = absolute_end.min(patch_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let source_from = usize::try_from(overlap_start - absolute_start).unwrap();
        let patch_from = usize::try_from(overlap_start - patch.offset).unwrap();
        let count = usize::try_from(overlap_end - overlap_start).unwrap();
        if bytes[source_from..source_from + count] != patch.expected[patch_from..patch_from + count]
        {
            return Err(std::io::Error::other(
                "PeerTube fragment metadata changed between segments",
            ));
        }
        bytes[source_from..source_from + count]
            .copy_from_slice(&patch.replacement[patch_from..patch_from + count]);
    }
    Ok(())
}

fn block_hash(origin: &str, start: u64, len: u64) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    origin.hash(&mut hasher);
    start.hash(&mut hasher);
    len.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::hls_media_playlist_url;

    #[test]
    fn derives_peer_tube_playlist_url_without_dropping_query() {
        assert_eq!(
            hls_media_playlist_url("https://peer.example/hls/v-480-fragmented.mp4?token=abc"),
            Some("https://peer.example/hls/v-480.m3u8?token=abc".to_owned())
        );
    }
}
