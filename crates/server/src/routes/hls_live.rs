//! The live half of the HLS proxy: an ephemeral, bounded segment cache.
//!
//! A VOD rendition is one immutable file we range-cache on disk forever (see
//! [`super::hls`]). A live stream is the opposite in every respect: `PeerTube`
//! writes each segment as a separate MPEG-TS file, advertises a sliding window
//! of them, and **deletes** the ones that fall out of it. Caching those the VOD
//! way would grow without bound — a 24/7 broadcast is an infinite file — while
//! caching nothing would multiply origin load by the number of viewers, which
//! is exactly what the rest of this stack exists to avoid.
//!
//! So live segments live in memory, briefly. That is enough to hold the whole
//! advertised window for every broadcast anyone is watching, which is all a
//! cache can usefully do here: N viewers at the live edge want the *same* few
//! segments within the same few seconds, and nobody ever wants the ones the
//! origin has already deleted. The cache is bounded by total bytes and by age,
//! evicts oldest-first, and is dropped wholesale for a broadcast whose media
//! sequence restarts — the signal that a permanent live began a new session and
//! is about to reuse the segment names of the last one.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::sync::RecoverableMutex as _;

/// Total live-segment bytes held across every broadcast. A 4 s segment of
/// ordinary 720p live is well under a megabyte, so this holds the full window
/// of many simultaneous streams; the cap only exists so that a pathological
/// number of them cannot grow the process without limit.
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Ceiling on a single cached segment. A live segment is a few seconds of
/// video; anything wildly larger is a broken or hostile origin, and is served
/// through without being cached rather than being allowed to evict everything
/// else.
pub const MAX_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
/// How long a segment stays useful. Past the live edge this only serves
/// viewers seeking back inside the DVR window; the origin's own window is
/// usually longer, but holding hours of video in memory is not the trade this
/// cache is here to make.
const ENTRY_TTL: Duration = Duration::from_mins(5);

struct Entry {
    media_id: i64,
    bytes: Arc<Vec<u8>>,
    stored_at: Instant,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    /// Insertion order, for oldest-first eviction. May name entries that are
    /// already gone; those are skipped when evicting.
    order: VecDeque<String>,
    bytes: usize,
    /// Which URLs belong to which broadcast, so a session restart can drop
    /// exactly that broadcast's segments.
    by_media: HashMap<i64, HashSet<String>>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache::default()));

impl Cache {
    fn forget(&mut self, url: &str) {
        if let Some(entry) = self.entries.remove(url) {
            self.bytes = self.bytes.saturating_sub(entry.bytes.len());
            if let Some(urls) = self.by_media.get_mut(&entry.media_id) {
                urls.remove(url);
                if urls.is_empty() {
                    self.by_media.remove(&entry.media_id);
                }
            }
        }
    }

    /// Drops entries until the cache is back under the byte cap, oldest first.
    fn evict_to_fit(&mut self) {
        while self.bytes > MAX_CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.forget(&oldest);
        }
    }
}

/// A cached live segment, if it is present and still fresh.
pub fn get(url: &str) -> Option<Arc<Vec<u8>>> {
    let mut cache = CACHE.lock_or_recover();
    let expired = cache
        .entries
        .get(url)
        .is_some_and(|entry| entry.stored_at.elapsed() >= ENTRY_TTL);
    if expired {
        cache.forget(url);
        return None;
    }
    cache.entries.get(url).map(|entry| Arc::clone(&entry.bytes))
}

/// Stores one live segment, evicting oldest-first to stay under the cap.
/// Segments over [`MAX_SEGMENT_BYTES`] are refused (the caller still serves
/// them; they simply are not remembered).
pub fn put(media_id: i64, url: &str, bytes: Arc<Vec<u8>>) {
    if bytes.len() as u64 > MAX_SEGMENT_BYTES {
        return;
    }
    let mut cache = CACHE.lock_or_recover();
    cache.forget(url); // replacing: drop the old accounting first
    cache.bytes += bytes.len();
    cache.entries.insert(
        url.to_owned(),
        Entry {
            media_id,
            bytes,
            stored_at: Instant::now(),
        },
    );
    cache.order.push_back(url.to_owned());
    cache
        .by_media
        .entry(media_id)
        .or_default()
        .insert(url.to_owned());
    cache.evict_to_fit();
}

/// Drops every cached segment of one broadcast — called when its media
/// sequence restarts, which means a new session is about to publish different
/// video under the segment names the last one used.
pub fn purge_media(media_id: i64) {
    let mut cache = CACHE.lock_or_recover();
    let Some(urls) = cache.by_media.remove(&media_id) else {
        return;
    };
    for url in urls {
        if let Some(entry) = cache.entries.remove(&url) {
            cache.bytes = cache.bytes.saturating_sub(entry.bytes.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Session tracking
// ---------------------------------------------------------------------------

/// Last `#EXT-X-MEDIA-SEQUENCE` seen per (media, playlist URL).
static SEQUENCES: LazyLock<Mutex<HashMap<(i64, String), u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Bound on the sequence map; one entry per rendition of each live anyone is
/// watching, so this is only a backstop.
const SEQUENCES_MAX: usize = 4_096;

/// Notes a live media playlist's sequence number, returning true when it went
/// *backwards* — the unambiguous mark of a new broadcast session, since ffmpeg
/// starts every one at zero. A permanent live reuses both its master URL and
/// its segment filenames across sessions, so without this the cache could
/// answer a request for `0-000000.ts` with the previous stream's video.
pub fn sequence_restarted(media_id: i64, playlist_url: &str, sequence: u64) -> bool {
    let mut seen = SEQUENCES.lock_or_recover();
    let key = (media_id, playlist_url.to_owned());
    let restarted = seen.get(&key).is_some_and(|last| sequence < *last);
    if seen.len() >= SEQUENCES_MAX && !seen.contains_key(&key) {
        seen.clear();
    }
    seen.insert(key, sequence);
    restarted
}

/// The `#EXT-X-MEDIA-SEQUENCE` of a media playlist, if it declares one. Only
/// live playlists do in practice; a VOD playlist starts at an implicit zero.
#[must_use]
pub fn media_sequence(body: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The media type of an HLS segment, from its filename. Live segments are
/// MPEG-TS; VOD segments are fragmented MP4. Getting this wrong breaks native
/// HLS playback (Safari/iOS), which trusts the header rather than sniffing.
#[must_use]
pub fn segment_content_type(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some(ext) if ext.eq_ignore_ascii_case("ts") => "video/mp2t",
        Some(ext) if ext.eq_ignore_ascii_case("aac") => "audio/aac",
        Some(ext) if ext.eq_ignore_ascii_case("vtt") => "text/vtt",
        _ => "video/mp4",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        let mut cache = CACHE.lock_or_recover();
        *cache = Cache::default();
        SEQUENCES.lock_or_recover().clear();
    }

    #[test]
    fn caches_and_purges_per_broadcast() {
        reset();
        put(1, "https://p/1/0-000000.ts", Arc::new(vec![7; 64]));
        put(2, "https://p/2/0-000000.ts", Arc::new(vec![9; 64]));
        assert_eq!(get("https://p/1/0-000000.ts").unwrap().len(), 64);
        // A restart drops only the broadcast that restarted.
        purge_media(1);
        assert!(get("https://p/1/0-000000.ts").is_none());
        assert!(get("https://p/2/0-000000.ts").is_some());
    }

    #[test]
    fn a_restarted_sequence_is_detected_once() {
        reset();
        assert!(
            !sequence_restarted(1, "a.m3u8", 0),
            "first sight is not a restart"
        );
        assert!(!sequence_restarted(1, "a.m3u8", 7), "advancing is normal");
        assert!(
            sequence_restarted(1, "a.m3u8", 0),
            "back to zero is a new session"
        );
        assert!(
            !sequence_restarted(1, "a.m3u8", 1),
            "and then it advances again"
        );
    }

    #[test]
    fn oversized_segments_are_not_remembered() {
        reset();
        let huge = Arc::new(vec![0_u8; usize::try_from(MAX_SEGMENT_BYTES).unwrap() + 1]);
        put(1, "https://p/1/huge.ts", huge);
        assert!(get("https://p/1/huge.ts").is_none());
    }

    #[test]
    fn segment_types_follow_the_extension() {
        assert_eq!(segment_content_type("https://p/0-000001.ts"), "video/mp2t");
        assert_eq!(
            segment_content_type("https://p/v-fragmented.mp4"),
            "video/mp4"
        );
        // Query strings must not be mistaken for an extension.
        assert_eq!(segment_content_type("https://p/a.ts?x=1"), "video/mp2t");
    }

    #[test]
    fn media_sequence_is_read_from_the_playlist() {
        let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:42\n0-000042.ts\n";
        assert_eq!(media_sequence(body), Some(42));
        assert_eq!(media_sequence("#EXTM3U\n#EXT-X-ENDLIST\n"), None);
    }
}
