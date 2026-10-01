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

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::sync::RecoverableMutex as _;

/// Total live-segment bytes held across every broadcast. A 4 s segment of
/// ordinary 720p live is well under a megabyte, so this holds the full window
/// of many simultaneous streams; the cap only exists so that a pathological
/// number of them cannot grow the process without limit.
const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;
/// Byte admission alone does not bound the metadata of tiny hostile segments.
const MAX_CACHE_ENTRIES: usize = 4_096;
const MAX_CACHE_URL_BYTES: usize = 8_192;
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
    sha256: String,
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
        // Replacement, expiry and purge must release their ordering metadata,
        // not leave tombstones accumulating forever below the byte ceiling.
        self.order.retain(|key| key != url);
    }

    /// Drops entries until the cache is back under the byte cap, oldest first.
    fn evict_to_fit(&mut self) {
        while self.bytes > MAX_CACHE_BYTES || self.entries.len() > MAX_CACHE_ENTRIES {
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
    if bytes.is_empty() || bytes.len() as u64 > MAX_SEGMENT_BYTES || url.len() > MAX_CACHE_URL_BYTES
    {
        return;
    }
    // Only complete, directly origin-fetched segments enter this cache. Hash
    // outside the lock, once, not on every viewer's checksum request.
    let sha256 = Sha256::digest(bytes.as_slice()).iter().fold(
        String::with_capacity(64),
        |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    );
    let mut cache = CACHE.lock_or_recover();
    let expired: Vec<_> = cache
        .entries
        .iter()
        .filter(|(_, entry)| entry.stored_at.elapsed() >= ENTRY_TTL)
        .map(|(url, _)| url.clone())
        .collect();
    for url in expired {
        cache.forget(&url);
    }
    cache.forget(url); // replacing: drop the old accounting first
    cache.bytes += bytes.len();
    cache.entries.insert(
        url.to_owned(),
        Entry {
            media_id,
            bytes,
            stored_at: Instant::now(),
            sha256,
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

/// Supplement a live origin's delayed checksum publication with complete
/// origin-fetched bytes. Scope to the current media and canonical directory;
/// ambiguous basenames must never pick an arbitrary rendition's digest.
pub fn checksums(media_id: i64, prefix: &str) -> serde_json::Map<String, serde_json::Value> {
    let cache = CACHE.lock_or_recover();
    let mut hashes = serde_json::Map::new();
    for (source, entry) in &cache.entries {
        if entry.media_id != media_id
            || entry.stored_at.elapsed() >= ENTRY_TTL
            || !source.starts_with(prefix)
        {
            continue;
        }
        let Ok(source) = url::Url::parse(source) else {
            continue;
        };
        let Some(name) = source.path_segments().and_then(Iterator::last) else {
            continue;
        };
        let digest = serde_json::Value::String(entry.sha256.clone());
        match hashes.entry(name.to_owned()) {
            serde_json::map::Entry::Vacant(slot) => {
                slot.insert(digest);
            }
            serde_json::map::Entry::Occupied(mut slot) if slot.get() != &digest => {
                slot.insert(serde_json::Value::Null);
            }
            serde_json::map::Entry::Occupied(_) => {}
        }
    }
    hashes.retain(|_, value| !value.is_null());
    hashes
}

/// Drops every cached segment of one broadcast — called when its media
/// sequence restarts, which means a new session is about to publish different
/// video under the segment names the last one used.
pub fn purge_media(media_id: i64) {
    let mut cache = CACHE.lock_or_recover();
    let Some(urls) = cache.by_media.get(&media_id).cloned() else {
        return;
    };
    for url in urls {
        cache.forget(&url);
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

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock_or_recover();
        let mut cache = CACHE.lock_or_recover();
        *cache = Cache::default();
        SEQUENCES.lock_or_recover().clear();
        guard
    }

    #[test]
    fn caches_and_purges_per_broadcast() {
        let _test = reset();
        put(1, "https://p/1/0-000000.ts", Arc::new(vec![7; 64]));
        put(2, "https://p/2/0-000000.ts", Arc::new(vec![9; 64]));
        assert_eq!(get("https://p/1/0-000000.ts").unwrap().len(), 64);
        // A restart drops only the broadcast that restarted.
        purge_media(1);
        assert!(get("https://p/1/0-000000.ts").is_none());
        assert!(get("https://p/2/0-000000.ts").is_some());
    }

    #[test]
    fn checksum_supplements_are_scoped_fresh_and_unambiguous() {
        let _test = reset();
        let prefix = "https://p/live/";
        put(1, "https://p/live/fresh.ts", Arc::new(vec![1]));
        put(2, "https://p/live/foreign.ts", Arc::new(vec![1]));
        put(1, "https://p/other/other.ts", Arc::new(vec![1]));
        put(1, "https://p/live/a/same.ts", Arc::new(vec![1]));
        put(1, "https://p/live/b/same.ts", Arc::new(vec![2]));
        put(1, "https://p/live/expired.ts", Arc::new(vec![1]));
        CACHE
            .lock_or_recover()
            .entries
            .get_mut("https://p/live/expired.ts")
            .unwrap()
            .stored_at = Instant::now().checked_sub(ENTRY_TTL).unwrap();
        let hashes = checksums(1, prefix);
        assert_eq!(hashes.len(), 1);
        assert_eq!(
            hashes["fresh.ts"],
            "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a"
        );
        purge_media(1);
        assert!(checksums(1, prefix).is_empty());
    }

    #[test]
    fn a_restarted_sequence_is_detected_once() {
        let _test = reset();
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
        let _test = reset();
        let huge = Arc::new(vec![0_u8; usize::try_from(MAX_SEGMENT_BYTES).unwrap() + 1]);
        put(1, "https://p/1/huge.ts", huge);
        assert!(get("https://p/1/huge.ts").is_none());
    }

    #[test]
    fn tiny_segments_and_repeated_replacements_bound_all_metadata() {
        let _test = reset();
        for _ in 0..MAX_CACHE_ENTRIES * 2 {
            put(1, "https://p/repeated.ts", Arc::new(vec![1]));
        }
        assert_eq!(CACHE.lock_or_recover().order.len(), 1);
        for n in 0..MAX_CACHE_ENTRIES + 10 {
            put(2, &format!("https://p/{n}.ts"), Arc::new(vec![2]));
        }
        {
            let cache = CACHE.lock_or_recover();
            assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
            assert_eq!(cache.order.len(), MAX_CACHE_ENTRIES);
            assert_eq!(cache.bytes, MAX_CACHE_ENTRIES);
            assert_eq!(cache.by_media.len(), 1);
        }
        purge_media(2);
        let cache = CACHE.lock_or_recover();
        assert!(cache.entries.is_empty() && cache.order.is_empty() && cache.by_media.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn insertion_reclaims_expired_entries_without_waiting_for_their_readers() {
        let _test = reset();
        put(1, "https://p/expired.ts", Arc::new(vec![3; 16]));
        CACHE
            .lock_or_recover()
            .entries
            .get_mut("https://p/expired.ts")
            .unwrap()
            .stored_at = Instant::now().checked_sub(ENTRY_TTL).unwrap();
        put(2, "https://p/fresh.ts", Arc::new(vec![4; 8]));
        let cache = CACHE.lock_or_recover();
        assert_eq!(cache.bytes, 8);
        assert_eq!(cache.order.len(), 1);
        assert!(!cache.by_media.contains_key(&1));
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
