//! Bounded checkpoints, sharing the paired store's ownership, writer and byte
//! budget. Missing mounts are never recreated. A busy writer skips a tick;
//! ingest does not queue snapshots or wait for a multi-GB media copy.
use super::{MirroredDiskStore, TemporaryCopy, invalid};
use crate::routes::native_live_hls::{valid_segment, valid_uuid};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const MAX_PLAYLIST: u64 = 256 * 1024;
const MAX_SEGMENT: u64 = 64 * 1024 * 1024;
const RECORDING_OFFSET: &str = ".plamenu-recording-offset";
pub(crate) const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(20);

impl MirroredDiskStore {
    pub(super) fn checked_live_dir(
        &self,
        root: &Path,
        media: i64,
        session: &str,
        create: bool,
    ) -> io::Result<PathBuf> {
        self.check_root(root)?;
        if media <= 0 || !valid_uuid(session) {
            return Err(invalid("invalid live generation"));
        }
        let mut path = root.to_path_buf();
        for part in ["native-live", &media.to_string(), session] {
            let parent = path.clone();
            path.push(part);
            if create {
                match std::fs::create_dir(&path) {
                    Ok(()) => {
                        std::fs::File::open(parent)?.sync_all()?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(invalid("live directory must not be a symlink"));
            }
        }
        Ok(path)
    }

    // Copy a finite immutable file/prefix in 64-KiB chunks. The temporary is
    // exclusive, private and removed on cancellation; publication is last.
    async fn live_copy(&self, source: &Path, destination: &Path, bytes: u64) -> io::Result<()> {
        let source = Self::key_path(
            source.parent().ok_or_else(|| invalid("source parent"))?,
            source
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("source name"))?,
        )?;
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("destination parent"))?;
        let meta = tokio::fs::symlink_metadata(destination).await;
        if meta.is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            return Err(invalid("checkpoint destination is not regular"));
        }
        self.admission(bytes)?;
        let temporary = parent.join(format!(
            ".plamenu-live-{}.tmp",
            crate::media_processing::storage_stem(0)
        ));
        let cleanup = TemporaryCopy(temporary.clone());
        let mut input = tokio::fs::File::open(source).await?.take(bytes);
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(&temporary).await?;
        let began = tokio::time::Instant::now();
        let mut copied = 0;
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let n = input.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            output.write_all(&chunk[..n]).await?;
            copied += n as u64;
            self.pace(began, copied).await;
        }
        if copied != bytes {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        output.sync_all().await?;
        drop(output);
        tokio::fs::rename(&temporary, destination).await?;
        Self::sync_root(parent.to_path_buf()).await?;
        drop(cleanup);
        Ok(())
    }

    async fn live_atomic(&self, directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
        let destination = directory.join(name);
        if tokio::fs::symlink_metadata(&destination)
            .await
            .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
        {
            return Err(invalid("checkpoint metadata is not regular"));
        }
        self.admission(bytes.len() as u64)?;
        let temporary = directory.join(format!(
            ".plamenu-live-{}.tmp",
            crate::media_processing::storage_stem(0)
        ));
        let cleanup = TemporaryCopy(temporary.clone());
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, destination).await?;
        Self::sync_root(directory.to_path_buf()).await?;
        drop(cleanup);
        Ok(())
    }

    pub(super) async fn live_checkpoint(&self, media: i64, session: &str) -> io::Result<()> {
        let _guard = self
            .writer
            .try_lock()
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        tokio::time::timeout(CHECKPOINT_TIMEOUT, async {
            let source = self.checked_live_dir(&self.primary_root, media, session, false)?;
            let destination = self.checked_live_dir(&self.replica_root, media, session, true)?;
            // Recording first, so a rapidly rolling HLS window cannot starve
            // recoverable recording fragments on every snapshot attempt.
            self.recording_checkpoint(&source, &destination).await?;
            let playlist = source.join("index.m3u8");
            let meta = tokio::fs::symlink_metadata(&playlist).await?;
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_PLAYLIST {
                return Err(invalid("invalid live playlist"));
            }
            if !meta
                .modified()?
                .elapsed()
                .is_ok_and(|age| age <= Duration::from_secs(30))
            {
                return Err(invalid("stale primary live playlist"));
            }
            let bytes = read_playlist(&source).await?;
            let names = playlist_names(&bytes)?;
            let previous = match read_playlist(&destination).await {
                Ok(bytes) => playlist_names(&bytes)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => HashSet::new(),
                Err(error) => return Err(error),
            };
            // Failed snapshots may have committed some segments without their
            // playlist. Bound those leftovers even if later ticks also fail.
            self.prune_live(&destination, &names, &previous).await?;
            for name in &names {
                let input = Self::key_path(&source, name)?;
                let meta = tokio::fs::symlink_metadata(&input).await?;
                if !meta.is_file()
                    || meta.file_type().is_symlink()
                    || !(1..=MAX_SEGMENT).contains(&meta.len())
                {
                    return Err(invalid("invalid live segment"));
                }
                let target = Self::key_path(&destination, name)?;
                // Encoder names are immutable inside one UUID. A completed
                // copy is never overwritten or assumed to be a partial file.
                if tokio::fs::try_exists(&target).await? {
                    if tokio::fs::metadata(&target).await?.len() != meta.len() {
                        return Err(invalid("immutable live segment length changed"));
                    }
                } else {
                    self.live_copy(&input, &target, meta.len()).await?;
                }
            }
            // Publish only once every referenced segment is durable. A failed
            // snapshot retains the prior playlist, not a dangling new window.
            self.live_atomic(&destination, "index.m3u8", &bytes).await?;
            self.prune_live(&destination, &names, &previous).await?;
            Self::sync_root(destination).await
        })
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
    }

    async fn prune_live(
        &self,
        directory: &Path,
        names: &HashSet<String>,
        previous: &HashSet<String>,
    ) -> io::Result<()> {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let old_temporary = name
                .strip_prefix(".plamenu-live-")
                .and_then(|value| value.strip_suffix(".tmp"))
                .is_some_and(|stem| {
                    stem.len() == 33
                        && stem.starts_with('0')
                        && stem.bytes().all(|b| b.is_ascii_hexdigit())
                })
                && entry
                    .metadata()
                    .await?
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age >= Duration::from_hours(48));
            if (valid_segment(&name) && !names.contains(&name) && !previous.contains(&name))
                || old_temporary
            {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }

    async fn recording_checkpoint(&self, source: &Path, destination: &Path) -> io::Result<()> {
        let recording = Self::key_path(source, "replay.mp4")?;
        let mut input = match tokio::fs::File::open(recording).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let offset = committed_offset(destination).await?;
        let end = fragment_end(&mut input, offset).await?;
        if end == offset {
            return Ok(());
        }
        self.admission(end - offset)?;
        let path = Self::key_path(destination, "replay.mp4")?;
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(path).await?;
        if output.metadata().await?.len() < offset {
            return Err(invalid("checkpoint shorter than committed recording"));
        }
        // Discard only a tail not covered by the durable offset. Crash or
        // cancellation between fsync and marker commit is safe to retry.
        output.set_len(offset).await?;
        output.seek(io::SeekFrom::Start(offset)).await?;
        input.seek(io::SeekFrom::Start(offset)).await?;
        let mut input = input.take(end - offset);
        let mut chunk = vec![0; 64 * 1024];
        let began = tokio::time::Instant::now();
        let mut copied = 0;
        loop {
            let n = input.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            output.write_all(&chunk[..n]).await?;
            copied += n as u64;
            self.pace(began, copied).await;
        }
        if copied != end - offset {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        output.sync_all().await?;
        drop(output);
        Self::sync_root(destination.to_path_buf()).await?;
        self.live_atomic(destination, RECORDING_OFFSET, end.to_string().as_bytes())
            .await
    }

    pub(super) async fn live_restore(&self, media: i64, session: &str) -> io::Result<bool> {
        let _guard = self.writer.lock().await;
        let source = match self.checked_live_dir(&self.replica_root, media, session, false) {
            Ok(path) => path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let offset = committed_offset(&source).await?;
        if offset == 0 {
            return Ok(false);
        }
        let destination = self.checked_live_dir(&self.primary_root, media, session, true)?;
        let target = Self::key_path(&destination, "replay.mp4")?;
        if tokio::fs::try_exists(&target).await? {
            return Ok(false);
        }
        self.live_copy(&source.join("replay.mp4"), &target, offset)
            .await?;
        Ok(true)
    }
}

pub(super) async fn read_playlist(directory: &Path) -> io::Result<Vec<u8>> {
    let path = MirroredDiskStore::key_path(directory, "index.m3u8")?;
    let mut file = tokio::fs::File::open(path).await?.take(MAX_PLAYLIST + 1);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_PLAYLIST {
        return Err(invalid("live playlist exceeds limit"));
    }
    Ok(bytes)
}

fn playlist_names(bytes: &[u8]) -> io::Result<HashSet<String>> {
    if bytes.len() as u64 > MAX_PLAYLIST {
        return Err(invalid("playlist too large"));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| invalid("playlist UTF-8"))?;
    if !source.starts_with("#EXTM3U\n") {
        return Err(invalid("playlist header"));
    }
    let mut names = HashSet::new();
    for line in source
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if !valid_segment(line) || !names.insert(line.to_owned()) || names.len() > 2000 {
            return Err(invalid("playlist segment names"));
        }
    }
    if names.is_empty() {
        return Err(invalid("empty live playlist"));
    }
    Ok(names)
}

async fn committed_offset(directory: &Path) -> io::Result<u64> {
    let path = directory.join(RECORDING_OFFSET);
    let meta = match tokio::fs::symlink_metadata(&path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 20 {
        return Err(invalid("recording checkpoint offset"));
    }
    tokio::fs::read_to_string(path)
        .await?
        .parse()
        .map_err(|_| invalid("recording checkpoint offset"))
}

/// Scan headers only, never payloads. Commit init + complete moof/mdat pairs;
/// an incomplete box (including an extended header) belongs to the next tick.
async fn fragment_end(file: &mut tokio::fs::File, offset: u64) -> io::Result<u64> {
    let length = file.metadata().await?.len();
    if length < offset {
        return Err(invalid("recording shrank behind checkpoint"));
    }
    let mut position = offset;
    let mut committed = offset;
    let mut initialized = offset > 0;
    let mut fragment = false;
    for _ in 0..4096 {
        if length - position < 8 {
            break;
        }
        file.seek(io::SeekFrom::Start(position)).await?;
        let mut header = [0; 16];
        file.read_exact(&mut header[..8]).await?;
        let short = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let (size, header_size) = if short == 1 {
            if length - position < 16 {
                break;
            }
            file.read_exact(&mut header[8..]).await?;
            (
                u64::from_be_bytes(header[8..].try_into().expect("eight bytes")),
                16,
            )
        } else {
            (u64::from(short), 8)
        };
        // Size zero extends to EOF and is not a closed fragmented-MP4 box.
        if size == 0 {
            break;
        }
        if size < header_size {
            return Err(invalid("invalid MP4 box size"));
        }
        if size > length - position {
            break;
        }
        match &header[4..8] {
            b"moov" if offset == 0 && !fragment => initialized = true,
            b"moof" if initialized && !fragment => fragment = true,
            b"mdat" if initialized && fragment => {
                fragment = false;
                committed = position + size;
            }
            b"ftyp" | b"free" | b"sidx" | b"mfra" | b"mfro" if !fragment => {}
            _ => return Err(invalid("unexpected fragmented MP4 box")),
        }
        position += size;
    }
    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MediaStore;
    const SESSION: &str = "12345678-1234-1234-1234-123456789abc";
    fn fixture() -> (tempfile::TempDir, MirroredDiskStore, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let replica = temp.path().join("replica");
        std::fs::create_dir(&replica).unwrap();
        let store = MirroredDiskStore::new(primary, replica, 64 * 1024 * 1024, 0).unwrap();
        let source = store
            .checked_live_dir(&store.primary_root, 42, SESSION, true)
            .unwrap();
        let destination = store
            .checked_live_dir(&store.replica_root, 42, SESSION, true)
            .unwrap();
        (temp, store, source, destination)
    }
    fn mp4_box(kind: &[u8], bytes: &[u8]) -> Vec<u8> {
        assert_eq!(kind.len(), 4);
        let mut result = u32::try_from(bytes.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        result.extend(kind);
        result.extend(bytes);
        result
    }
    fn movie() -> Vec<u8> {
        [
            mp4_box(b"ftyp", b"isom"),
            mp4_box(b"moov", b"init"),
            mp4_box(b"moof", b"fragment"),
            mp4_box(b"mdat", b"closed media"),
        ]
        .concat()
    }
    fn playlist(names: &[&str]) -> Vec<u8> {
        format!("#EXTM3U\n#EXT-X-VERSION:3\n{}\n", names.join("\n")).into_bytes()
    }
    #[tokio::test]
    async fn fragment_boundary_excludes_partial_tail_and_recovers_retry() {
        let (temp, _, _, _) = fixture();
        let path = temp.path().join("fragment.mp4");
        let complete = movie();
        let next = [mp4_box(b"moof", b"next"), mp4_box(b"mdat", b"second media")].concat();
        tokio::fs::write(&path, [&complete[..], &next[..next.len() - 2]].concat())
            .await
            .unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert_eq!(
            fragment_end(&mut file, 0).await.unwrap(),
            complete.len() as u64
        );
        tokio::fs::write(&path, [&complete[..], &next[..]].concat())
            .await
            .unwrap();
        assert_eq!(
            fragment_end(&mut file, complete.len() as u64)
                .await
                .unwrap(),
            (complete.len() + next.len()) as u64
        );
        assert!(
            fragment_end(&mut file, (complete.len() + next.len() + 1) as u64)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn extended_headers_and_invalid_boxes_are_bounded() {
        let (temp, _, _, _) = fixture();
        let path = temp.path().join("extended.mp4");
        let complete = movie();
        let mut extended = 1_u32.to_be_bytes().to_vec();
        extended.extend(b"free");
        extended.extend(16_u64.to_be_bytes());
        tokio::fs::write(&path, [&complete[..], &extended[..12]].concat())
            .await
            .unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        assert_eq!(
            fragment_end(&mut file, 0).await.unwrap(),
            complete.len() as u64
        );
        tokio::fs::write(&path, [&complete[..], &extended[..]].concat())
            .await
            .unwrap();
        assert_eq!(
            fragment_end(&mut file, 0).await.unwrap(),
            complete.len() as u64
        );
        tokio::fs::write(&path, b"\0\0\0\x04mdat").await.unwrap();
        assert!(fragment_end(&mut file, 0).await.is_err());
    }
    #[tokio::test]
    async fn recording_offset_excludes_cancelled_append_tail_and_restores_only_prefix() {
        let (_temp, store, source, destination) = fixture();
        let bytes = movie();
        tokio::fs::write(source.join("replay.mp4"), &bytes)
            .await
            .unwrap();
        store
            .recording_checkpoint(&source, &destination)
            .await
            .unwrap();
        assert_eq!(
            committed_offset(&destination).await.unwrap(),
            bytes.len() as u64
        );
        let next = [
            mp4_box(b"moof", b"next"),
            mp4_box(b"mdat", b"new complete media"),
        ]
        .concat();
        let all = [&bytes[..], &next[..]].concat();
        tokio::fs::write(source.join("replay.mp4"), &all)
            .await
            .unwrap();
        store
            .recording_checkpoint(&source, &destination)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(destination.join("replay.mp4"))
                .await
                .unwrap(),
            all
        );
        let mut replica = tokio::fs::OpenOptions::new()
            .append(true)
            .open(destination.join("replay.mp4"))
            .await
            .unwrap();
        replica.write_all(b"UNCOMMITTED").await.unwrap();
        drop(replica);
        tokio::fs::remove_dir_all(&source).await.unwrap();
        assert!(
            store
                .restore_live_recording(42, SESSION.into())
                .await
                .unwrap()
        );
        assert_eq!(
            tokio::fs::read(source.join("replay.mp4")).await.unwrap(),
            all
        );
        assert!(
            !store
                .restore_live_recording(42, SESSION.into())
                .await
                .unwrap()
        );
        store
            .recording_checkpoint(&source, &destination)
            .await
            .unwrap();
        // With no new fragment, the tail remains unacknowledged, never copied.
        assert_eq!(
            committed_offset(&destination).await.unwrap(),
            all.len() as u64
        );
    }
    #[tokio::test]
    async fn playlist_commit_is_last_and_retains_previous_window_for_readers() {
        let (_temp, store, source, destination) = fixture();
        let first = playlist(&["segment000001.ts"]);
        tokio::fs::write(source.join("segment000001.ts"), b"first")
            .await
            .unwrap();
        tokio::fs::write(source.join("index.m3u8"), &first)
            .await
            .unwrap();
        store.checkpoint_live(42, SESSION.into()).await.unwrap();
        assert_eq!(
            tokio::fs::read(destination.join("index.m3u8"))
                .await
                .unwrap(),
            first
        );
        tokio::fs::write(source.join("index.m3u8"), playlist(&["segment000002.ts"]))
            .await
            .unwrap();
        assert!(store.checkpoint_live(42, SESSION.into()).await.is_err());
        assert_eq!(
            tokio::fs::read(destination.join("index.m3u8"))
                .await
                .unwrap(),
            first
        );
        tokio::fs::write(source.join("segment000002.ts"), b"second")
            .await
            .unwrap();
        store.checkpoint_live(42, SESSION.into()).await.unwrap();
        assert!(destination.join("segment000001.ts").exists());
        tokio::fs::write(source.join("index.m3u8"), playlist(&["segment000003.ts"]))
            .await
            .unwrap();
        tokio::fs::write(source.join("segment000003.ts"), b"third")
            .await
            .unwrap();
        store.checkpoint_live(42, SESSION.into()).await.unwrap();
        assert!(!destination.join("segment000001.ts").exists());
        assert!(destination.join("segment000002.ts").exists());
        assert_eq!(store.list().await.unwrap(), Vec::<String>::new());
    }
    #[tokio::test]
    async fn busy_writer_missing_mount_and_free_floor_leave_previous_checkpoint() {
        let (_temp, mut store, source, destination) = fixture();
        tokio::fs::write(source.join("index.m3u8"), playlist(&["segment000001.ts"]))
            .await
            .unwrap();
        tokio::fs::write(source.join("segment000001.ts"), b"first")
            .await
            .unwrap();
        let guard = store.writer.lock().await;
        assert_eq!(
            store
                .checkpoint_live(42, SESSION.into())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(guard);
        store.min_free = u64::MAX;
        assert_eq!(
            store
                .checkpoint_live(42, SESSION.into())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!destination.join("index.m3u8").exists());
        store.min_free = 0;
        tokio::fs::remove_file(store.replica_root.join(super::super::MARKER))
            .await
            .unwrap();
        assert!(store.live_replica_dir(42, SESSION).is_none());
        assert!(store.checkpoint_live(42, SESSION.into()).await.is_err());
    }
    #[tokio::test]
    async fn reconcile_preserves_recoverable_recording_and_unknown_files() {
        let (_temp, store, _, destination) = fixture();
        tokio::fs::write(destination.join("replay.mp4"), movie())
            .await
            .unwrap();
        let notes = store.replica_root.join("native-live/operator-notes");
        tokio::fs::write(&notes, b"preserve").await.unwrap();
        assert_eq!(
            store
                .reconcile_live_checkpoints(vec![(42, SESSION.into())])
                .await
                .unwrap(),
            0
        );
        assert!(destination.exists());
        assert_eq!(store.reconcile_live_checkpoints(vec![]).await.unwrap(), 1);
        assert!(!destination.exists());
        assert!(notes.exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_components_and_segments_never_escape_roots() {
        let (temp, store, source, destination) = fixture();
        assert!(
            store
                .checked_live_dir(&store.primary_root, 42, "../escape", false)
                .is_err()
        );
        assert!(playlist_names(b"#EXTM3U\n../escape.ts\n").is_err());
        assert!(playlist_names(b"#EXTM3U\nsegment000001.ts\nsegment000001.ts\n").is_err());
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"private").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("segment000001.ts")).unwrap();
        std::fs::write(source.join("index.m3u8"), playlist(&["segment000001.ts"])).unwrap();
        assert!(store.checkpoint_live(42, SESSION.into()).await.is_err());
        std::fs::remove_dir(&destination).unwrap();
        std::os::unix::fs::symlink(temp.path(), &destination).unwrap();
        assert!(store.live_replica_dir(42, SESSION).is_none());
        assert!(
            store
                .delete_live_checkpoint(42, SESSION.into())
                .await
                .is_err()
        );
        assert!(outside.exists());
    }
}
