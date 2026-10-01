//! Public, generation-fenced HLS reads for Plamenu-origin broadcasts.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use tokio::io::AsyncReadExt;

use crate::error::ApiError;
use crate::state::AppState;
use crate::sync::RecoverableMutex as _;

const M3U8: &str = "application/vnd.apple.mpegurl";
const MAX_PLAYLIST: u64 = 256 * 1024;
const MAX_SEGMENT: u64 = 64 * 1024 * 1024;
const MAX_REPLICA_AGE: Duration = Duration::from_secs(30);

pub(crate) fn primary_directory(state: &AppState, media_id: i64, session: &str) -> Option<PathBuf> {
    if media_id <= 0 || !valid_uuid(session) {
        return None;
    }
    let mut path = state.config.media_dir.clone();
    for component in ["native-live", &media_id.to_string(), session] {
        path.push(component);
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return None;
        }
    }
    Some(path)
}

/// A snapshot is playable only while fresh; canonical DB authority/session
/// checks happen before callers can use it. A lost publisher cannot leave an
/// indefinitely on-air frozen replica playlist.
fn replica_directory(state: &AppState, media_id: i64, session: &str) -> Option<PathBuf> {
    let path = state.media.live_replica_dir(media_id, session)?;
    fresh_replica_playlist(&path).then_some(path)
}

fn fresh_replica_playlist(path: &std::path::Path) -> bool {
    let Some(name) = crate::storage_mirror::checkpoint_playlist_file(path) else {
        return false;
    };
    std::fs::symlink_metadata(path.join(name)).is_ok_and(|meta| {
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta
                .modified()
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age <= MAX_REPLICA_AGE))
    })
}

pub(super) async fn playlist_directory(
    state: &AppState,
    media_id: i64,
    session: &str,
) -> Option<PathBuf> {
    let mut selected = None;
    let mut highest = 0;
    for path in [
        primary_directory(state, media_id, session),
        replica_directory(state, media_id, session),
    ]
    .into_iter()
    .flatten()
    {
        if !ready_files(&path).await {
            continue;
        }
        let Some(name) = crate::storage_mirror::checkpoint_playlist_file(&path) else {
            continue;
        };
        let Ok(source) = tokio::fs::read_to_string(path.join(name)).await else {
            continue;
        };
        let sequence = source
            .lines()
            .find_map(|line| {
                line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")?
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0);
        // A DVR list does not roll its MEDIA-SEQUENCE until it fills. A
        // repaired, disabled primary can therefore still say sequence zero
        // while its surviving replica has dozens more closed segments.
        let edge = source
            .lines()
            .filter(|line| valid_segment(line))
            .filter_map(|line| {
                line.strip_prefix("segment")?
                    .strip_suffix(".ts")?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(sequence);
        if selected.is_none() || edge > highest {
            highest = edge;
            selected = Some(path);
        }
    }
    selected
}

/// A native encoder marks on-air before committing its first HLS segment.
/// Shared progressive startup must wait for an atomic playlist containing a
/// complete segment, not feed `FFmpeg` a temporarily missing child playlist.
pub(super) async fn ready(state: &AppState, media_id: i64, session: &str) -> bool {
    playlist_directory(state, media_id, session).await.is_some()
}

async fn ready_files(root: &std::path::Path) -> bool {
    let Some(name) = crate::storage_mirror::checkpoint_playlist_file(root) else {
        return false;
    };
    let path = root.join(name);
    let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_PLAYLIST {
        return false;
    }
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return false;
    };
    if bytes.len() > usize::try_from(MAX_PLAYLIST).unwrap_or(0) {
        return false;
    }
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return false;
    };
    if !source.starts_with("#EXTM3U") {
        return false;
    }
    for line in source
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if !valid_segment(line) {
            return false;
        }
        if let Ok(meta) = tokio::fs::symlink_metadata(root.join(line)).await
            && meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() > 0
            && meta.len() <= MAX_SEGMENT
        {
            return true;
        }
    }
    false
}
const MAX_HASHED_SEGMENTS: usize = 2_000;
const MAX_HASH_SESSIONS: usize = 32;
type SegmentHashes = Arc<Mutex<HashMap<String, String>>>;
type HashSessions = Mutex<HashMap<String, (Instant, SegmentHashes)>>;
static HASH_SESSIONS: LazyLock<HashSessions> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn session_hashes(media_id: i64, session: &str, directory: &std::path::Path) -> SegmentHashes {
    let mut sessions = HASH_SESSIONS.lock_or_recover();
    sessions.retain(|_, (at, _)| at.elapsed() < Duration::from_mins(30));
    let key = format!("{media_id}/{session}/{}", directory.display());
    if let Some((at, hashes)) = sessions.get_mut(&key) {
        *at = Instant::now();
        return Arc::clone(hashes);
    }
    if sessions.len() >= MAX_HASH_SESSIONS
        && let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, (at, _))| at)
            .map(|(key, _)| key.clone())
    {
        sessions.remove(&oldest);
    }
    let hashes = Arc::new(Mutex::new(HashMap::new()));
    sessions.insert(key, (Instant::now(), Arc::clone(&hashes)));
    hashes
}

/// Only segments named by the atomic `FFmpeg` playlist are hashed. File names
/// are monotonic within a session and the encoder commits them via temp-file
/// rename, so cached digests cannot refer to changing bytes. Each viewer of
/// the same session shares the cache and hashes a new segment at most once.
pub async fn sha256_manifest(
    state: &AppState,
    media_id: i64,
) -> Result<Option<Response>, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let Some(session) = plamenu_db::native_live::public_session(&state.pool, media_id).await?
    else {
        return Ok(None);
    };
    if !valid_uuid(&session) {
        return Err(ApiError::NotFound);
    }
    let directory = playlist_directory(state, media_id, &session)
        .await
        .ok_or(ApiError::NotFound)?;
    let path = directory.join(
        crate::storage_mirror::checkpoint_playlist_file(&directory).ok_or(ApiError::NotFound)?,
    );
    let names = match tokio::fs::symlink_metadata(&path).await {
        Ok(meta)
            if meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_PLAYLIST =>
        {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|_| ApiError::NotFound)?;
            if !matches!(u64::try_from(bytes.len()), Ok(len) if len <= MAX_PLAYLIST) {
                return Err(ApiError::NotFound);
            }
            let source = String::from_utf8(bytes).map_err(|_| ApiError::NotFound)?;
            if !source.starts_with("#EXTM3U") {
                return Err(ApiError::NotFound);
            }
            let names: Vec<String> = source
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect();
            if names.len() > MAX_HASHED_SEGMENTS || names.iter().any(|name| !valid_segment(name)) {
                return Err(ApiError::NotFound);
            }
            names
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        _ => return Err(ApiError::NotFound),
    };
    let hashes = session_hashes(media_id, &session, &directory);
    let replica = replica_directory(state, media_id, &session);
    let manifest = tokio::task::spawn_blocking(move || {
        hash_segments_with_replica(&directory, replica.as_deref(), &names, &hashes)
    })
    .await
    .map_err(|e| ApiError::Internal(Box::new(e)))?
    .ok_or(ApiError::NotFound)?;
    Ok(Some(
        (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            manifest,
        )
            .into_response(),
    ))
}

#[cfg(test)]
fn hash_segments(
    directory: &std::path::Path,
    names: &[String],
    cache: &Mutex<HashMap<String, String>>,
) -> Option<Vec<u8>> {
    hash_segments_with_replica(directory, None, names, cache)
}

fn hash_segments_with_replica(
    directory: &std::path::Path,
    replica: Option<&std::path::Path>,
    names: &[String],
    cache: &Mutex<HashMap<String, String>>,
) -> Option<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let mut hashes = cache.lock_or_recover();
    let current: HashSet<_> = names.iter().map(String::as_str).collect();
    hashes.retain(|name, _| current.contains(name.as_str()));
    let mut chunk = [0_u8; 16 * 1024];
    for name in names {
        if hashes.contains_key(name) {
            continue;
        }
        let Some((path, meta)) = std::iter::once(directory)
            .chain(replica)
            .find_map(|directory| {
                let path = directory.join(name);
                let meta = std::fs::symlink_metadata(&path).ok()?;
                (meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() > 0
                    && meta.len() <= MAX_SEGMENT)
                    .then_some((path, meta))
            })
        else {
            continue;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let mut digest = Sha256::new();
        let mut read_bytes = 0_u64;
        while let Ok(size) = file.read(&mut chunk) {
            if size == 0 {
                if read_bytes != meta.len() {
                    break;
                }
                let mut hex = String::with_capacity(64);
                for byte in digest.finalize() {
                    write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
                }
                hashes.insert(name.clone(), hex);
                break;
            }
            digest.update(&chunk[..size]);
            read_bytes += size as u64;
            if read_bytes > MAX_SEGMENT {
                break;
            }
        }
    }
    let mut output = serde_json::Map::new();
    for name in names {
        if let Some(hash) = hashes.get(name) {
            output.insert(name.clone(), serde_json::json!(hash));
        }
    }
    let bytes = serde_json::to_vec(&output).ok()?;
    (bytes.len() <= 2 * 1024 * 1024).then_some(bytes)
}

pub async fn master(state: &AppState, media_id: i64) -> Result<Option<Response>, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let Some(session) = plamenu_db::native_live::public_session(&state.pool, media_id).await?
    else {
        return Ok(None);
    };
    let body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=4000000\n/media/hls/{media_id}/local/{session}/index.m3u8\n"
    );
    Ok(Some(
        (
            [
                (header::CONTENT_TYPE, M3U8),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
    ))
}

pub async fn child(
    State(state): State<AppState>,
    Path((media_id, session, name)): Path<(i64, String, String)>,
) -> Result<Response, ApiError> {
    if plamenu_db::media::public_access_blocked(&state.pool, media_id).await? {
        return Err(ApiError::NotFound);
    }
    let current = plamenu_db::native_live::public_session(&state.pool, media_id).await?;
    if current.as_deref() != Some(session.as_str()) || !valid_uuid(&session) {
        return Err(ApiError::NotFound);
    }
    let playlist = name == "index.m3u8";
    if !playlist && !valid_segment(&name) {
        return Err(ApiError::NotFound);
    }
    let mut bytes = None;
    let current_directory = playlist_directory(&state, media_id, &session).await;
    for directory in [
        current_directory,
        primary_directory(&state, media_id, &session),
        replica_directory(&state, media_id, &session),
    ]
    .into_iter()
    .flatten()
    {
        let path = directory.join(if playlist {
            let Some(file) = crate::storage_mirror::checkpoint_playlist_file(&directory) else {
                continue;
            };
            file
        } else {
            &name
        });
        let limit = if playlist { MAX_PLAYLIST } else { MAX_SEGMENT };
        if !tokio::fs::symlink_metadata(&path).await.is_ok_and(|meta| {
            meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.len() > 0
                && meta.len() <= limit
        }) {
            continue;
        }
        // Bound the read as well as metadata; no growing/replaced file can
        // turn one HLS request into an unbounded allocation.
        if let Ok(file) = tokio::fs::File::open(path).await {
            let mut value = Vec::new();
            if file.take(limit + 1).read_to_end(&mut value).await.is_ok()
                && value.len() as u64 <= limit
            {
                bytes = Some(value);
                break;
            }
        }
    }
    let bytes = bytes.ok_or(ApiError::NotFound)?;
    if playlist {
        let source = String::from_utf8(bytes).map_err(|_| ApiError::NotFound)?;
        let mut output = String::with_capacity(source.len() + 512);
        for line in source.lines() {
            if line.starts_with('#') || line.is_empty() {
                output.push_str(line);
            } else if valid_segment(line) {
                write!(output, "/media/hls/{media_id}/local/{session}/{line}")
                    .expect("writing to a String cannot fail");
            } else {
                return Err(ApiError::NotFound);
            }
            output.push('\n');
        }
        Ok((
            [
                (header::CONTENT_TYPE, M3U8),
                (header::CACHE_CONTROL, "no-store"),
            ],
            output,
        )
            .into_response())
    } else {
        Ok((
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "public, max-age=30"),
            ],
            bytes,
        )
            .into_response())
    }
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

pub(crate) fn valid_segment(name: &str) -> bool {
    name.strip_prefix("segment")
        .and_then(|tail| tail.strip_suffix(".ts"))
        .is_some_and(|digits| {
            (6..=12).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_and_future_replica_playlists_cannot_serve_live_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.m3u8");
        assert!(!fresh_replica_playlist(dir.path()));
        let file = std::fs::File::create(path).unwrap();
        assert!(fresh_replica_playlist(dir.path()));
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(31))
            .unwrap();
        assert!(!fresh_replica_playlist(dir.path()));
        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(31))
            .unwrap();
        assert!(!fresh_replica_playlist(dir.path()));
    }

    #[test]
    fn integrity_manifest_uses_replica_for_missing_primary_segment() {
        let source = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        std::fs::write(replica.path().join("segment000001.ts"), b"complete segment").unwrap();
        let cache = Mutex::new(HashMap::new());
        let manifest = hash_segments_with_replica(
            source.path(),
            Some(replica.path()),
            &["segment000001.ts".into()],
            &cache,
        )
        .unwrap();
        let object: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(object["segment000001.ts"].as_str().unwrap().len(), 64);
        assert_eq!(
            manifest,
            hash_segments(
                replica.path(),
                &["segment000001.ts".into()],
                &Mutex::new(HashMap::new())
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn progressive_startup_requires_an_atomic_playlist_and_complete_safe_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!ready_files(root).await);
        std::fs::write(root.join("index.m3u8"), "#EXTM3U\n").unwrap();
        assert!(!ready_files(root).await);
        std::fs::write(
            root.join("index.m3u8"),
            "#EXTM3U\n#EXTINF:1,\nsegment000001.ts\n",
        )
        .unwrap();
        assert!(!ready_files(root).await);
        std::fs::write(root.join("segment000001.ts"), b"").unwrap();
        assert!(!ready_files(root).await);
        std::fs::write(root.join("segment000001.ts"), b"complete atomic segment").unwrap();
        assert!(ready_files(root).await);
        std::fs::write(root.join("index.m3u8"), "#EXTM3U\n../segment000001.ts\n").unwrap();
        assert!(!ready_files(root).await);
        std::fs::write(
            root.join("index.m3u8"),
            "not a playlist\nsegment000001.ts\n",
        )
        .unwrap();
        assert!(!ready_files(root).await);
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join("index.m3u8")).unwrap();
            std::os::unix::fs::symlink(root.join("segment000001.ts"), root.join("index.m3u8"))
                .unwrap();
            assert!(!ready_files(root).await);
        }
    }

    #[test]
    fn child_paths_reject_traversal() {
        assert!(valid_segment("segment000001.ts"));
        assert!(!valid_segment("../segment000001.ts"));
        assert!(!valid_segment("segment000001.ts/../secret"));
        assert!(valid_uuid("12345678-1234-1234-1234-123456789abc"));
        assert!(!valid_uuid("../../../../evil"));
    }

    #[test]
    fn live_hash_cache_tracks_and_prunes_committed_segments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segment000001.ts"), b"abc").unwrap();
        let cache = Mutex::new(HashMap::new());
        let manifest = hash_segments(dir.path(), &["segment000001.ts".into()], &cache).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(
            manifest["segment000001.ts"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(cache.lock_or_recover().len(), 1);
        assert_eq!(hash_segments(dir.path(), &[], &cache).unwrap(), b"{}");
        assert!(cache.lock_or_recover().is_empty());
    }
}
