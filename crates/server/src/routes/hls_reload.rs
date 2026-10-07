//! Bounded closed-segment blocking reload primitives. Lifetime eligibility is
//! established separately from persisted, current remote authority.
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::sync::RecoverableMutex as _;

pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
const MAX_SEGMENTS: usize = 2048;
const MAX_WAITERS: usize = 256;
const MAX_PER_KEY: usize = 32;
const MAX_KEYS: usize = 64;
const MAX_MASTERS: usize = 16;
const MAX_FETCHES: usize = 16;
const FETCHES_PER_SECOND: usize = 64;
const MAX_URL_BYTES: usize = 4096;
pub const MAX_WAIT: Duration = Duration::from_secs(15);
pub const REFRESH: Duration = Duration::from_secs(1);

pub struct Snapshot {
    pub body: Box<str>,
    pub closed: Option<ClosedPlaylist>,
    pub ended: bool,
    pub rewritten: bool,
}

/// A cached failure shares the same cooldown as a successful refresh. A failed
/// winner cannot turn its queued viewers into independent origin retries.
#[derive(Default)]
pub struct Flight {
    pub revision: i64,
    pub refreshed: Option<std::time::Instant>,
    pub snapshot: Option<Arc<Snapshot>>,
    pub observed: Option<ClosedPlaylist>,
    pub outcome_recorded: bool,
    pub origin_failed: bool,
}

pub struct Ladder {
    pub children: Vec<String>,
    pub fingerprint: [u8; 32],
    pub target: Duration,
    pub ended: bool,
    pub event: bool,
}
pub struct MasterSnapshot {
    pub body: Box<str>,
    pub ladder: Option<Ladder>,
}
#[derive(Default)]
pub struct MasterFlight {
    pub revision: i64,
    pub refreshed: Option<std::time::Instant>,
    pub snapshot: Option<Arc<MasterSnapshot>>,
    pub origin_failed: bool,
    pub outcome_recorded: bool,
}
type MasterFlights = HashMap<(i64, String), Arc<tokio::sync::Mutex<MasterFlight>>>;
static MASTERS: LazyLock<Mutex<MasterFlights>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn master_flight(
    media: i64,
    url: &str,
) -> Result<Arc<tokio::sync::Mutex<MasterFlight>>, ApiError> {
    if url.len() > MAX_URL_BYTES {
        return Err(unavailable());
    }
    let key = (media, url.to_owned());
    let mut flights = MASTERS.lock_or_recover();
    if let Some(flight) = flights.get(&key) {
        return Ok(Arc::clone(flight));
    }
    if flights.len() >= MAX_MASTERS {
        flights.retain(|_, flight| {
            Arc::strong_count(flight) > 1
                || flight
                    .try_lock()
                    .is_ok_and(|f| f.refreshed.is_some_and(|at| at.elapsed() < REFRESH))
        });
    }
    if flights.len() >= MAX_MASTERS {
        return Err(unavailable());
    }
    let flight = Arc::new(tokio::sync::Mutex::new(MasterFlight::default()));
    flights.insert(key, Arc::clone(&flight));
    Ok(flight)
}

static FETCH_SLOTS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_FETCHES)));
struct FetchRate {
    started: std::time::Instant,
    count: usize,
}
impl Default for FetchRate {
    fn default() -> Self {
        Self {
            started: std::time::Instant::now(),
            count: 0,
        }
    }
}
static FETCH_RATE: LazyLock<Mutex<FetchRate>> = LazyLock::new(|| Mutex::new(FetchRate::default()));

/// Metadata fetches are admitted without a queue, including unused renditions
/// read by preflight. Fixed one-second windows allow a burst of at most 128
/// starts straddling a boundary, with at most 16 concurrent upstream reads.
pub fn fetch_permit() -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    let permit = Arc::clone(&FETCH_SLOTS)
        .try_acquire_owned()
        .map_err(|_| unavailable())?;
    let mut rate = FETCH_RATE.lock_or_recover();
    if rate.started.elapsed() >= REFRESH {
        *rate = FetchRate::default();
    }
    if rate.count >= FETCHES_PER_SECOND {
        return Err(unavailable());
    }
    rate.count += 1;
    Ok(permit)
}

type Flights = HashMap<(i64, String), Arc<tokio::sync::Mutex<Flight>>>;
static FLIGHTS: LazyLock<Mutex<Flights>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn flight(media: i64, url: &str) -> Result<Arc<tokio::sync::Mutex<Flight>>, ApiError> {
    if url.len() > MAX_URL_BYTES {
        return Err(unavailable());
    }
    let key = (media, url.to_owned());
    let mut flights = FLIGHTS.lock_or_recover();
    if let Some(flight) = flights.get(&key) {
        return Ok(Arc::clone(flight));
    }
    if flights.len() >= MAX_KEYS {
        // Never evict an active flight, which would allow a second fetch for
        // the same key. Dormant entries hold at most one bounded snapshot.
        flights.retain(|_, flight| {
            Arc::strong_count(flight) > 1
                || flight
                    .try_lock()
                    .is_ok_and(|f| f.refreshed.is_some_and(|at| at.elapsed() < REFRESH))
        });
    }
    if flights.len() >= MAX_KEYS {
        return Err(unavailable());
    }
    let flight = Arc::new(tokio::sync::Mutex::new(Flight::default()));
    flights.insert(key, Arc::clone(&flight));
    Ok(flight)
}

#[derive(Default, Deserialize)]
pub struct Directives {
    #[serde(rename = "_HLS_msn")]
    msn: Option<String>,
    #[serde(rename = "_HLS_part")]
    part: Option<String>,
}

impl Directives {
    pub fn requested(&self) -> bool {
        self.msn.is_some() || self.part.is_some()
    }
    pub fn msn(&self, ended: bool) -> Result<Option<u64>, ApiError> {
        if ended {
            return Ok(None);
        }
        // Partial segments are deliberately unsupported. The protocol allows
        // an error instead of waiting; never falsely satisfy a partial request.
        if self.part.is_some() {
            return Err(ApiError::BadRequest(
                "Partial HLS reload is unsupported".into(),
            ));
        }
        match self.msn.as_deref() {
            None => Ok(None),
            Some(value) => decimal(value)
                .map(Some)
                .ok_or_else(|| ApiError::BadRequest("Invalid HLS media sequence".into())),
        }
    }
}

fn decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

#[derive(Debug, Clone)]
pub struct ClosedPlaylist {
    pub first: u64,
    pub last: u64,
    pub target: Duration,
    pub ended: bool,
    pub event: bool,
    identities: Arc<[[u8; 32]]>,
}

impl ClosedPlaylist {
    /// Only complete, ordinary TS media playlists qualify initially. Complex
    /// origin capabilities remain on their existing ordinary serving path.
    pub fn parse(body: &str) -> Option<Self> {
        let body = body.trim_start_matches('\u{feff}');
        if body.lines().next().map(str::trim) != Some("#EXTM3U") {
            return None;
        }
        let mut discontinuity = 0_u64;
        let mut discontinuity_seen = false;
        let mut max_duration = 0.0_f64;
        let mut first = 0;
        let mut sequence_seen = false;
        let mut target = None;
        let mut version = None;
        let mut playlist_type = None;
        let mut ended = false;
        let mut pending = None;
        let mut identities = Vec::new();
        for line in body.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
                if playlist_type.is_some() || !matches!(value, "EVENT" | "VOD") {
                    return None;
                }
                playlist_type = Some(value);
            } else if let Some(value) = line.strip_prefix("#EXT-X-VERSION:") {
                if version.is_some() {
                    return None;
                }
                version = Some(decimal(value).filter(|n| (1..=13).contains(n))?);
            } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
                if discontinuity_seen || !identities.is_empty() {
                    return None;
                }
                discontinuity = decimal(value)?;
                discontinuity_seen = true;
            } else if line == "#EXT-X-DISCONTINUITY" {
                discontinuity = discontinuity.checked_add(1)?;
            } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                if sequence_seen || !identities.is_empty() {
                    return None;
                }
                first = decimal(value)?;
                sequence_seen = true;
            } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                if target.is_some() {
                    return None;
                }
                let seconds = decimal(value)?;
                if !(1..=60).contains(&seconds) {
                    return None;
                }
                target = Some(Duration::from_secs(seconds));
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                if pending.is_some() || ended {
                    return None;
                }
                let duration = value.split(',').next()?.parse::<f64>().ok()?;
                if !duration.is_finite() || duration <= 0.0 {
                    return None;
                }
                max_duration = max_duration.max(duration.round());
                pending = Some(duration);
            } else if line == "#EXT-X-ENDLIST" {
                if ended || pending.is_some() {
                    return None;
                }
                ended = true;
            } else if unsupported(line) {
                return None;
            } else if !line.is_empty() && !line.starts_with('#') {
                let duration = pending.take()?;
                if std::path::Path::new(line.split('?').next()?).extension()
                    != Some(std::ffi::OsStr::new("ts"))
                    || identities.len() >= MAX_SEGMENTS
                {
                    return None;
                }
                let mut digest = Sha256::new();
                digest.update(discontinuity.to_le_bytes());
                digest.update(duration.to_le_bytes());
                digest.update([0]);
                digest.update(line.as_bytes());
                identities.push(digest.finalize().into());
            }
        }
        if pending.is_some() || identities.is_empty() || (playlist_type == Some("VOD") && !ended) {
            return None;
        }
        let last = first.checked_add(u64::try_from(identities.len()).ok()?.checked_sub(1)?)?;
        let target = target?;
        if max_duration > target.as_secs_f64() || last > 9_007_199_254_740_989 {
            return None;
        }
        Some(Self {
            first,
            last,
            target,
            ended,
            event: playlist_type == Some("EVENT"),
            identities: Arc::from(identities.into_boxed_slice()),
        })
    }

    pub fn fits(&self, rewritten_bytes: usize) -> bool {
        rewritten_bytes.saturating_add(self.identities.len() * 32) <= MAX_SNAPSHOT_BYTES
    }

    pub fn restarted(&self, previous: &Self) -> bool {
        if self.first < previous.first
            || self.last < previous.last
            || self.target != previous.target
            || self.event != previous.event
            || (previous.event && self.first != previous.first)
        {
            return true;
        }
        let first = self.first.max(previous.first);
        let last = self.last.min(previous.last);
        (first..=last).any(|sequence| {
            let current = usize::try_from(sequence - self.first).expect("bounded segment count");
            let old = usize::try_from(sequence - previous.first).expect("bounded segment count");
            self.identities[current] != previous.identities[old]
        })
    }

    pub fn validate_msn(&self, msn: u64) -> Result<(), ApiError> {
        if msn > self.last.saturating_add(2) {
            Err(ApiError::BadRequest(
                "HLS media sequence is too far ahead".into(),
            ))
        } else {
            Ok(())
        }
    }
}

fn unsupported(line: &str) -> bool {
    [
        "#EXT-X-STREAM-INF:",
        "#EXT-X-I-FRAMES-ONLY",
        "#EXT-X-PART",
        "#EXT-X-SKIP:",
        "#EXT-X-PRELOAD-HINT:",
        "#EXT-X-RENDITION-REPORT:",
        "#EXT-X-BYTERANGE:",
        "#EXT-X-MAP:",
        "#EXT-X-KEY:",
        "#EXT-X-GAP",
        "#EXT-X-SERVER-CONTROL:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

#[derive(Default)]
struct Admission {
    total: usize,
    keys: HashMap<(i64, String), usize>,
}
static ADMISSION: LazyLock<Mutex<Admission>> = LazyLock::new(|| Mutex::new(Admission::default()));

pub struct Waiter {
    key: (i64, String),
}
impl Waiter {
    pub fn acquire(media: i64, url: &str) -> Result<Self, ApiError> {
        if url.len() > MAX_URL_BYTES {
            return Err(unavailable());
        }
        let key = (media, url.to_owned());
        let mut state = ADMISSION.lock_or_recover();
        let count = state.keys.get(&key).copied().unwrap_or(0);
        if state.total >= MAX_WAITERS
            || count >= MAX_PER_KEY
            || (count == 0 && state.keys.len() >= MAX_KEYS)
        {
            return Err(unavailable());
        }
        *state.keys.entry(key.clone()).or_default() += 1;
        state.total += 1;
        Ok(Self { key })
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        let mut state = ADMISSION.lock_or_recover();
        if let Some(count) = state.keys.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                state.keys.remove(&self.key);
            }
            state.total -= 1;
        }
    }
}
pub fn unavailable() -> ApiError {
    ApiError::ServiceUnavailable("Live playlist reload unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;
    fn playlist(first: u64, count: usize) -> String {
        let mut body = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{first}\n#EXT-X-PLAYLIST-TYPE:EVENT\n"
        );
        for n in 0..count {
            writeln!(
                body,
                "#EXTINF:4.0,\n{}.ts",
                first.saturating_add(u64::try_from(n).unwrap())
            )
            .unwrap();
        }
        body
    }
    #[test]
    fn closed_event_and_sliding_boundaries_use_complete_segment_count() {
        for first in [0, 100] {
            let parsed = ClosedPlaylist::parse(&playlist(first, 6)).unwrap();
            assert_eq!((parsed.first, parsed.last), (first, first + 5));
            for msn in [0, first + 5, first + 6, first + 7] {
                assert!(parsed.validate_msn(msn).is_ok());
            }
            assert!(parsed.validate_msn(first + 8).is_err());
        }
    }
    #[test]
    fn malformed_and_unsupported_histories_do_not_advertise_closed_reload() {
        let ordinary = playlist(0, 2);
        for tag in [
            "#EXT-X-PART-INF:PART-TARGET=0.2",
            "#EXT-X-SKIP:SKIPPED-SEGMENTS=1",
            "#EXT-X-SERVER-CONTROL:CAN-SKIP-UNTIL=24",
            "#EXT-X-MAP:URI=\"init.mp4\"",
            "#EXT-X-GAP",
        ] {
            assert!(ClosedPlaylist::parse(&(ordinary.clone() + tag + "\n")).is_none());
        }
        assert!(ClosedPlaylist::parse(&playlist(u64::MAX, 2)).is_none());
        assert!(ClosedPlaylist::parse(&(ordinary + "#EXTINF:4,\n")).is_none());
    }
    #[test]
    fn ended_history_ignores_valid_msn_and_part_directives() {
        let query = Directives {
            msn: Some("10".into()),
            part: Some("2".into()),
        };
        assert!(query.msn(false).is_err());
        assert_eq!(query.msn(true).unwrap(), None);
        for bad in ["", "-1", "+1", "1.0", "18446744073709551616"] {
            assert!(
                Directives {
                    msn: Some(bad.into()),
                    part: None
                }
                .msn(false)
                .is_err()
            );
        }
    }
    #[test]
    fn observable_restart_and_overlap_replacement_are_generation_changes() {
        let previous = ClosedPlaylist::parse(&playlist(0, 6)).unwrap();
        assert!(
            !ClosedPlaylist::parse(&playlist(0, 7))
                .unwrap()
                .restarted(&previous)
        );
        assert!(
            ClosedPlaylist::parse(&playlist(0, 3))
                .unwrap()
                .restarted(&previous)
        );
        let changed = playlist(0, 7).replace("5.ts", "replacement.ts");
        assert!(
            ClosedPlaylist::parse(&changed)
                .unwrap()
                .restarted(&previous)
        );
        assert!(
            ClosedPlaylist::parse(&playlist(0, 7).replace("0.ts", "earlier.ts"))
                .unwrap()
                .restarted(&previous)
        );
        assert!(
            ClosedPlaylist::parse(&playlist(0, 7))
                .unwrap()
                .restarted(&ClosedPlaylist::parse(&playlist(1, 6)).unwrap())
        );
    }
    #[test]
    fn snapshot_budget_includes_segment_identity_memory() {
        let parsed = ClosedPlaylist::parse(&playlist(0, 6)).unwrap();
        assert!(parsed.fits(MAX_SNAPSHOT_BYTES - 6 * 32));
        assert!(!parsed.fits(MAX_SNAPSHOT_BYTES - 6 * 32 + 1));
    }
    #[test]
    fn flight_keys_are_bounded_and_active_ownership_cannot_be_evicted() {
        let held = (0..MAX_KEYS)
            .map(|n| flight(993, &format!("https://owned.invalid/flight-{n}")).unwrap())
            .collect::<Vec<_>>();
        assert!(flight(993, "https://owned.invalid/extra").is_err());
        assert!(Arc::ptr_eq(
            &held[0],
            &flight(993, "https://owned.invalid/flight-0").unwrap()
        ));
        drop(held);
        assert!(flight(993, "https://owned.invalid/extra").is_ok());
    }
    #[test]
    fn metadata_admission_bounds_concurrent_reads_and_aggregate_starts() {
        let held = (0..MAX_FETCHES)
            .map(|_| fetch_permit().unwrap())
            .collect::<Vec<_>>();
        assert!(fetch_permit().is_err());
        drop(held);
        for _ in MAX_FETCHES..FETCHES_PER_SECOND {
            drop(fetch_permit().unwrap());
        }
        assert!(fetch_permit().is_err());
    }
    #[test]
    fn admission_bounds_keys_per_key_and_aggregate_and_releases_on_drop() {
        let mut guards = Vec::new();
        for n in 0..MAX_PER_KEY {
            guards.push(Waiter::acquire(991, "https://owned.invalid/one").unwrap());
            assert_eq!(guards.len(), n + 1);
        }
        assert!(Waiter::acquire(991, "https://owned.invalid/one").is_err());
        drop(guards.pop());
        guards.push(Waiter::acquire(991, "https://owned.invalid/one").unwrap());
        for key in 1..8 {
            for _ in 0..MAX_PER_KEY {
                guards.push(Waiter::acquire(991, &format!("https://owned.invalid/{key}")).unwrap());
            }
        }
        assert!(Waiter::acquire(991, "https://owned.invalid/extra").is_err());
        drop(guards);
        let keys = (0..MAX_KEYS)
            .map(|n| Waiter::acquire(992, &format!("https://owned.invalid/key-{n}")).unwrap())
            .collect::<Vec<_>>();
        assert!(Waiter::acquire(992, "https://owned.invalid/key-extra").is_err());
        drop(keys);
        assert!(Waiter::acquire(992, "https://owned.invalid/key-extra").is_ok());
        assert!(Waiter::acquire(992, &"x".repeat(MAX_URL_BYTES + 1)).is_err());
    }
}
