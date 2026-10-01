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
const DIRECT_WRITER: &str = ".plamenu-direct-live";
const DIRECT_MARKER: &[u8] = b"plamenu-native-direct-live-v1\n";
const DIRECT_PLAYLIST: &str = "index.durable.m3u8";
pub(crate) const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(20);

impl MirroredDiskStore {
    pub(super) async fn prepare_direct_writer(
        &self,
        media: i64,
        session: &str,
        reencode: bool,
    ) -> io::Result<crate::storage::LiveReplicaWriter> {
        #[cfg(not(target_os = "linux"))]
        return Err(io::ErrorKind::Unsupported.into());
        #[cfg(target_os = "linux")]
        {
            let _guard = self.writer.lock().await;
            self.admission(8 * 1024 * 1024)?;
            let directory = self.checked_live_dir(&self.replica_root, media, session, true)?;
            // Open before the marker disables the legacy copier. The pump
            // cannot silently lose its second writer between these steps.
            let directory_handle = std::fs::File::open(&directory)?;
            let reservation = Some(self.bandwidth.reserve_live(reencode)?);
            // Persist before dispatch: checkpoint workers must never append or
            // truncate a recording which the encoder writes independently.
            self.live_atomic(&directory, DIRECT_WRITER, DIRECT_MARKER)
                .await?;
            Ok(crate::storage::LiveReplicaWriter {
                directory,
                directory_handle,
                min_free_bytes: self.min_free,
                bandwidth: self.bandwidth.clone(),
                bandwidth_reservation: reservation,
            })
        }
    }

    async fn direct_recording_checkpoint(&self, directory: &Path) -> io::Result<()> {
        let recording = Self::key_path(directory, "replay.mp4")?;
        let mut input = match tokio::fs::File::open(&recording).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let offset = committed_offset(directory).await?;
        let end = fragment_end(&mut input, offset).await?;
        if end > offset {
            input.sync_all().await?;
            Self::sync_root(directory.to_path_buf()).await?;
            self.live_atomic(directory, RECORDING_OFFSET, end.to_string().as_bytes())
                .await?;
        }
        let playlist = read_playlist(directory).await?;
        let names = playlist_names(&playlist)?;
        let already_synced = match read_playlist_file(directory, DIRECT_PLAYLIST).await {
            Ok(previous) => playlist_names(&previous)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => return Err(error),
        };
        for name in names {
            let path = Self::key_path(directory, &name)?;
            let file = tokio::fs::File::open(path).await?;
            let metadata = file.metadata().await?;
            if metadata.len() == 0 || metadata.len() > MAX_SEGMENT {
                return Err(invalid("invalid direct live segment"));
            }
            // Monotonic closed segment names are immutable. Their earlier
            // durable playlist already acknowledged fsync; do not fsync an
            // entire thirty-minute window again on every two-second tick.
            if !already_synced.contains(&name) {
                file.sync_all().await?;
            }
        }
        self.live_atomic(directory, DIRECT_PLAYLIST, &playlist)
            .await?;
        Ok(())
    }

    pub(super) async fn best_recording(
        &self,
        media: i64,
        session: &str,
    ) -> io::Result<Option<PathBuf>> {
        let mut best = None;
        let mut longest = 0;
        for root in [&self.primary_root, &self.replica_root] {
            let Ok(directory) = self.checked_live_dir(root, media, session, false) else {
                continue;
            };
            let Ok(path) = Self::key_path(&directory, "replay.mp4") else {
                continue;
            };
            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                continue;
            };
            let end = if root == &self.replica_root {
                // This generation's private marker acknowledges a scanned,
                // fsynced prefix. Do not rescan a long recording from zero
                // with the per-tick 4096-box limit and compare a stale prefix.
                let Ok(offset) = committed_offset(&directory).await else {
                    continue;
                };
                let Ok(metadata) = file.metadata().await else {
                    continue;
                };
                if offset > metadata.len() {
                    continue;
                }
                offset
            } else {
                let Ok(Ok(end)) =
                    tokio::time::timeout(Duration::from_secs(5), recording_end(&mut file)).await
                else {
                    continue;
                };
                end
            };
            if end > longest {
                longest = end;
                best = Some(path);
            }
        }
        Ok(best)
    }
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
        let mut copied = 0;
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let n = input.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            self.bandwidth.transfer(n as u64).await?;
            output.write_all(&chunk[..n]).await?;
            copied += n as u64;
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
        // Metadata belongs to the checked replica generation. A failed
        // primary must not prevent that surviving copy acknowledging fsync.
        self.check_root(&self.replica_root)?;
        if crate::storage::available_bytes(&self.replica_root)?
            < self.min_free.saturating_add(bytes.len() as u64)
        {
            return Err(io::ErrorKind::StorageFull.into());
        }
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
        // Direct writers do not share mutable recording/index files with the
        // old copier and do not queue behind durable flat-key copies.
        if let Ok(directory) = self.checked_live_dir(&self.replica_root, media, session, false) {
            let marker = directory.join(DIRECT_WRITER);
            if let Ok(metadata) = tokio::fs::symlink_metadata(&marker).await {
                if !metadata.is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.len() != DIRECT_MARKER.len() as u64
                    || tokio::fs::read(marker).await? != DIRECT_MARKER
                {
                    return Err(invalid("invalid direct live ownership marker"));
                }
                return tokio::time::timeout(
                    CHECKPOINT_TIMEOUT,
                    self.direct_recording_checkpoint(&directory),
                )
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?;
            }
        }
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
        let mut copied = 0;
        loop {
            let n = input.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            self.bandwidth.transfer(n as u64).await?;
            output.write_all(&chunk[..n]).await?;
            copied += n as u64;
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

/// Direct encoders own their raw playlist; the second disk serves only its
/// separately fsynced snapshot. Legacy asynchronous checkpoints stay valid.
pub(crate) fn checkpoint_playlist_file(directory: &Path) -> Option<&'static str> {
    match std::fs::symlink_metadata(directory.join(DIRECT_WRITER)) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == DIRECT_MARKER.len() as u64
                && std::fs::read(directory.join(DIRECT_WRITER))
                    .ok()?
                    .as_slice()
                    == DIRECT_MARKER =>
        {
            Some(DIRECT_PLAYLIST)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some("index.m3u8"),
        Ok(_) | Err(_) => None,
    }
}

pub(super) async fn read_playlist(directory: &Path) -> io::Result<Vec<u8>> {
    read_playlist_file(directory, "index.m3u8").await
}

async fn read_playlist_file(directory: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = MirroredDiskStore::key_path(directory, name)?;
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

/// Final selection may cover many checkpoint batches, but reads only headers
/// and has both a finite box budget and an outer deadline. The incremental
/// checkpoint scanner still does at most 4096 boxes per tick.
async fn recording_end(file: &mut tokio::fs::File) -> io::Result<u64> {
    let mut offset = 0;
    for _ in 0..256 {
        let end = fragment_end(file, offset).await?;
        if end == offset {
            return Ok(end);
        }
        offset = end;
    }
    Err(invalid("recording exceeds bounded header scan"))
}

/// A closed native replay can have a partial tail after disk failure. Return
/// only complete fragments (and, on a replica, its acknowledged prefix),
/// without truncating or copying the retained recording.
pub(crate) async fn replay_prefix(path: &Path) -> io::Result<u64> {
    let mut file = tokio::fs::File::open(path).await?;
    let end = tokio::time::timeout(Duration::from_secs(5), recording_end(&mut file))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let directory = path.parent().ok_or_else(|| invalid("recording parent"))?;
    let end = match tokio::fs::symlink_metadata(directory.join(RECORDING_OFFSET)).await {
        Ok(_) => end.min(committed_offset(directory).await?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => end,
        Err(error) => return Err(error),
    };
    if end == 0 {
        return Err(invalid("recording has no complete fragment"));
    }
    Ok(end)
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
    async fn direct_checkpoint_never_copies_or_truncates_and_does_not_wait_for_flat_writer() {
        let (_temp, store, source, destination) = fixture();
        store
            .prepare_direct_writer(42, SESSION, false)
            .await
            .unwrap();
        let complete = movie();
        let next = [mp4_box(b"moof", b"next"), mp4_box(b"mdat", b"second media")].concat();
        let committed = [complete.clone(), next.clone()].concat();
        let recording = [committed.clone(), next[..next.len() - 2].to_vec()].concat();
        tokio::fs::write(source.join("replay.mp4"), complete)
            .await
            .unwrap();
        tokio::fs::write(destination.join("replay.mp4"), &recording)
            .await
            .unwrap();
        let manifest = playlist(&["segment000000.ts"]);
        tokio::fs::write(destination.join("index.m3u8"), &manifest)
            .await
            .unwrap();
        tokio::fs::write(destination.join("segment000000.ts"), b"closed test segment")
            .await
            .unwrap();
        let _busy = store.writer.lock().await;
        tokio::time::timeout(
            Duration::from_secs(2),
            store.checkpoint_live(42, SESSION.into()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            tokio::fs::read(destination.join("replay.mp4"))
                .await
                .unwrap(),
            recording
        );
        assert_eq!(
            committed_offset(&destination).await.unwrap(),
            committed.len() as u64
        );
        assert_eq!(
            tokio::fs::read(destination.join(DIRECT_PLAYLIST))
                .await
                .unwrap(),
            manifest
        );
        assert_eq!(
            checkpoint_playlist_file(&destination),
            Some(DIRECT_PLAYLIST)
        );
        assert_eq!(
            store.best_recording(42, SESSION).await.unwrap(),
            Some(destination.join("replay.mp4"))
        );
    }

    #[tokio::test]
    async fn invalid_direct_marker_fails_closed_without_running_the_old_recording_copier() {
        let (_temp, store, source, destination) = fixture();
        tokio::fs::write(source.join("replay.mp4"), movie())
            .await
            .unwrap();
        tokio::fs::write(destination.join("replay.mp4"), b"preserve encoder's file")
            .await
            .unwrap();
        tokio::fs::write(destination.join(DIRECT_WRITER), b"foreign")
            .await
            .unwrap();
        assert!(store.checkpoint_live(42, SESSION.into()).await.is_err());
        assert!(checkpoint_playlist_file(&destination).is_none());
        assert_eq!(
            tokio::fs::read(destination.join("replay.mp4"))
                .await
                .unwrap(),
            b"preserve encoder's file"
        );
    }
    #[tokio::test]
    async fn long_recording_selection_does_not_compare_two_truncated_scan_batches() {
        let (_temp, store, source, destination) = fixture();
        let mut bytes = movie();
        let fragment = [mp4_box(b"moof", b"next"), mp4_box(b"mdat", b"media")].concat();
        for _ in 0..3000 {
            bytes.extend(&fragment);
        }
        tokio::fs::write(source.join("replay.mp4"), &bytes)
            .await
            .unwrap();
        bytes.extend(&fragment);
        tokio::fs::write(destination.join("replay.mp4"), &bytes)
            .await
            .unwrap();
        tokio::fs::write(destination.join(RECORDING_OFFSET), bytes.len().to_string())
            .await
            .unwrap();
        assert_eq!(
            store.best_recording(42, SESSION).await.unwrap(),
            Some(destination.join("replay.mp4"))
        );
        bytes.extend(&fragment);
        tokio::fs::write(source.join("replay.mp4"), &bytes)
            .await
            .unwrap();
        assert_eq!(
            store.best_recording(42, SESSION).await.unwrap(),
            Some(source.join("replay.mp4"))
        );
        tokio::fs::write(
            destination.join(RECORDING_OFFSET),
            (bytes.len() + 1).to_string(),
        )
        .await
        .unwrap();
        assert_eq!(
            store.best_recording(42, SESSION).await.unwrap(),
            Some(source.join("replay.mp4"))
        );
    }
    #[tokio::test]
    async fn replay_prefix_preserves_partial_tail_and_never_exceeds_replica_acknowledgement() {
        let (_temp, _store, source, _) = fixture();
        let complete = movie();
        let mut bytes = complete.clone();
        bytes.extend(mp4_box(b"moof", b"next"));
        bytes.extend(mp4_box(b"mdat", b"incomplete")[..10].to_vec());
        let path = source.join("replay.mp4");
        tokio::fs::write(&path, &bytes).await.unwrap();
        assert_eq!(replay_prefix(&path).await.unwrap(), complete.len() as u64);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        tokio::fs::write(source.join(RECORDING_OFFSET), "0")
            .await
            .unwrap();
        assert!(replay_prefix(&path).await.is_err());
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
