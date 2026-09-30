//! Optional native disk replication of durable flat media keys.
//! Live HLS windows and complete recording fragments have bounded, asynchronous
//! checkpoints; they are not synchronous publisher failover or backups.
mod live;
mod rewind;
use crate::{
    federation::BoxFuture,
    storage::{LocalDiskStore, MediaRead, MediaStore, available_bytes},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

/// Successful writes acknowledge both disks. Reads prefer the primary and can
/// fall back to the replica; HTTP authority remains the caller's responsibility.
pub struct MirroredDiskStore {
    primary: LocalDiskStore,
    replica: LocalDiskStore,
    primary_root: PathBuf,
    replica_root: PathBuf,
    writer: Mutex<()>,
    bytes_per_second: u64,
    min_free: u64,
    marker: String,
}
const MARKER: &str = ".plamenu-mirror-owner";
const STAGING: &str = ".plamenu-mirror-tmp";

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
}

/// A missing/replaced mount must not be silently initialized as an empty disk.
/// A replica can only be initialized when empty; both roots retain the pairing.
fn paired_marker(primary: &Path, replica: &Path) -> std::io::Result<String> {
    let primary_marker = std::fs::read_to_string(primary.join(MARKER));
    let replica_marker = std::fs::read_to_string(replica.join(MARKER));
    match (primary_marker, replica_marker) {
        (Ok(a), Ok(b)) if a == b && a.starts_with("plamenu-mirror-v1\n") => Ok(a),
        (Err(a), Err(b))
            if a.kind() == std::io::ErrorKind::NotFound
                && b.kind() == std::io::ErrorKind::NotFound
                && std::fs::read_dir(replica)?.next().is_none() =>
        {
            let marker = format!(
                "plamenu-mirror-v1\n{}\n",
                crate::media_processing::storage_stem(0)
            );
            for root in [primary, replica] {
                use std::io::Write;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(root.join(MARKER))?;
                file.write_all(marker.as_bytes())?;
                file.sync_all()?;
                std::fs::File::open(root)?.sync_all()?;
            }
            Ok(marker)
        }
        _ => Err(invalid(
            "missing or mismatched mirror ownership; restore the paired disk, do not initialize a missing mount",
        )),
    }
}
impl MirroredDiskStore {
    pub fn new(
        primary: PathBuf,
        replica: PathBuf,
        bytes_per_second: u64,
        min_free: u64,
    ) -> std::io::Result<Self> {
        if bytes_per_second == 0 || !replica.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "replica must be an existing dedicated directory and bandwidth must be positive",
            ));
        }
        std::fs::create_dir_all(&primary)?;
        let primary_root = primary.canonicalize()?;
        let replica_root = replica.canonicalize()?;
        let primary_store = LocalDiskStore::new(primary)?;
        let replica_store = LocalDiskStore::new(replica)?;
        let primary = primary_root;
        let replica = replica_root;
        if primary.starts_with(&replica) || replica.starts_with(&primary) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "primary and replica must be independent directories",
            ));
        }
        let marker = paired_marker(&primary, &replica)?;
        Ok(Self {
            primary: primary_store,
            replica: replica_store,
            primary_root: primary,
            replica_root: replica,
            writer: Mutex::new(()),
            bytes_per_second,
            min_free,
            marker,
        })
    }
    fn key_path(root: &Path, key: &str) -> std::io::Result<PathBuf> {
        let mut parts = Path::new(key).components();
        if !matches!(parts.next(), Some(std::path::Component::Normal(_))) || parts.next().is_some()
        {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        if key.starts_with(".plamenu-") {
            return Err(invalid("reserved mirror key"));
        }
        let path = root.join(key);
        if std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(invalid("media key must be a regular file, never a symlink"));
        }
        Ok(path)
    }
    fn check_root(&self, root: &Path) -> std::io::Result<()> {
        let path = root.join(MARKER);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || std::fs::read_to_string(path)? != self.marker
        {
            return Err(invalid("mirror ownership marker changed"));
        }
        Ok(())
    }
    async fn sync_root(root: PathBuf) -> std::io::Result<()> {
        tokio::task::spawn_blocking(move || std::fs::File::open(root)?.sync_all())
            .await
            .map_err(std::io::Error::other)?
    }
    async fn sync_primary(&self, key: &str) -> std::io::Result<()> {
        tokio::fs::File::open(Self::key_path(&self.primary_root, key)?)
            .await?
            .sync_all()
            .await?;
        Self::sync_root(self.primary_root.clone()).await
    }
    async fn pace(&self, began: tokio::time::Instant, copied: u64) {
        let elapsed = Duration::from_secs(copied / self.bytes_per_second)
            + Duration::from_nanos(
                (copied % self.bytes_per_second).saturating_mul(1_000_000_000)
                    / self.bytes_per_second,
            );
        tokio::time::sleep_until(began + elapsed).await;
    }
    fn admission(&self, bytes: u64) -> std::io::Result<()> {
        for root in [&self.primary_root, &self.replica_root] {
            self.check_root(root)?;
            if available_bytes(root)? < self.min_free.saturating_add(bytes) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "mirrored storage free-space floor",
                ));
            }
        }
        Ok(())
    }
    async fn copy_between(&self, key: &str, from: &Path, to: &Path) -> std::io::Result<()> {
        self.check_root(from)?;
        self.check_root(to)?;
        let source = Self::key_path(from, key)?;
        let destination = Self::key_path(to, key)?;
        let staging = to.join(STAGING);
        if tokio::fs::symlink_metadata(&staging)
            .await
            .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(invalid("mirror staging must be a dedicated directory"));
        }
        tokio::fs::create_dir_all(&staging).await?;
        let temporary = staging.join(format!("{}.tmp", crate::media_processing::storage_stem(0)));
        let cleanup = TemporaryCopy(temporary.clone());
        let mut input = tokio::fs::File::open(source).await?;
        // Measure again after the primary write. Paired directories may share
        // a filesystem, and a staged replacement temporarily retains old data.
        if available_bytes(to)? < self.min_free.saturating_add(input.metadata().await?.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "mirror copy free-space floor",
            ));
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(&temporary).await?;
        let began = tokio::time::Instant::now();
        let mut copied = 0_u64;
        let mut chunk = vec![0_u8; 64 * 1024];
        loop {
            let count = input.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            output.write_all(&chunk[..count]).await?;
            copied += u64::try_from(count).unwrap_or(u64::MAX);
            self.pace(began, copied).await;
        }
        output.sync_all().await?;
        drop(output);
        tokio::fs::rename(&temporary, destination).await?;
        Self::sync_root(to.to_path_buf()).await?;
        drop(cleanup);
        Ok(())
    }
    /// Startup seeds existing primary keys serially under the same byte budget.
    /// Equal-sized files are compared, never assumed to be identical. A failed
    /// or interrupted seed is safe to rerun; no acknowledgement precedes it.
    pub async fn seed(&self) -> std::io::Result<usize> {
        let _guard = self.writer.lock().await;
        self.check_root(&self.primary_root)?;
        self.check_root(&self.replica_root)?;
        for root in [&self.primary_root, &self.replica_root] {
            self.scavenge_staging(root).await?;
        }
        let mut copied = 0;
        for key in self.primary.list().await? {
            if key.starts_with(".plamenu-") {
                continue;
            }
            let source = Self::key_path(&self.primary_root, &key)?;
            let size = tokio::fs::metadata(source).await?.len();
            self.admission(size)?;
            if !self.equal(&key).await? {
                self.copy_between(&key, &self.primary_root, &self.replica_root)
                    .await?;
                copied += 1;
            }
        }
        Ok(copied)
    }
    async fn scavenge_staging(&self, root: &Path) -> std::io::Result<()> {
        let staging = root.join(STAGING);
        if let Ok(metadata) = tokio::fs::symlink_metadata(&staging).await
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(invalid("mirror staging must be a dedicated directory"));
        }
        let mut entries = match tokio::fs::read_dir(staging).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let metadata = entry.metadata().await?;
            // Exactly our random staging format, regular files only, and no
            // recent copy (including concurrent CLI writes) is ever touched.
            if name.len() == 37
                && name.starts_with('0')
                && name.ends_with(".tmp")
                && name.as_bytes()[..33].iter().all(u8::is_ascii_hexdigit)
                && entry.file_type().await?.is_file()
                && metadata
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age > Duration::from_hours(48))
            {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }
    async fn equal(&self, key: &str) -> std::io::Result<bool> {
        let a = Self::key_path(&self.primary_root, key)?;
        let b = Self::key_path(&self.replica_root, key)?;
        let mut a = tokio::fs::File::open(a).await?;
        let mut b = match tokio::fs::File::open(b).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if a.metadata().await?.len() != b.metadata().await?.len() {
            return Ok(false);
        }
        let mut left = vec![0_u8; 64 * 1024];
        let mut right = vec![0_u8; 64 * 1024];
        loop {
            let n = a.read(&mut left).await?;
            if n == 0 {
                return Ok(true);
            }
            b.read_exact(&mut right[..n]).await?;
            if left[..n] != right[..n] {
                return Ok(false);
            }
        }
    }
}
struct TemporaryCopy(PathBuf);
impl Drop for TemporaryCopy {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let path = self.0.clone();
            runtime.spawn(async move {
                let _ = tokio::fs::remove_file(path).await;
            });
        }
    }
}
impl MediaStore for MirroredDiskStore {
    fn has_live_checkpoints(&self) -> bool {
        true
    }
    fn checkpoint_live(&self, media: i64, session: String) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move { self.live_checkpoint(media, &session).await })
    }
    fn live_replica_dir(&self, media: i64, session: &str) -> Option<PathBuf> {
        self.checked_live_dir(&self.replica_root, media, session, false)
            .ok()
    }
    fn live_rewind_source(
        &self,
        media: i64,
        session: String,
        slice: i32,
    ) -> BoxFuture<'_, std::io::Result<Option<crate::storage::LiveRewindSource>>> {
        Box::pin(async move { self.live_rewind(media, &session, slice).await })
    }
    fn restore_live_recording(
        &self,
        media: i64,
        session: String,
    ) -> BoxFuture<'_, std::io::Result<bool>> {
        Box::pin(async move { self.live_restore(media, &session).await })
    }
    fn delete_live_checkpoint(
        &self,
        media: i64,
        session: String,
    ) -> BoxFuture<'_, std::io::Result<()>> {
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            let path = match self.checked_live_dir(&self.replica_root, media, &session, false) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            tokio::fs::remove_dir_all(&path).await?;
            Self::sync_root(path.parent().expect("session parent").to_path_buf()).await
        })
    }
    fn reconcile_live_checkpoints(
        &self,
        retained: Vec<(i64, String)>,
    ) -> BoxFuture<'_, std::io::Result<usize>> {
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            self.check_root(&self.replica_root)?;
            crate::live_ingest::scavenge_crash_scratch(
                &self.replica_root,
                &retained
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>(),
            )
            .await
        })
    }
    fn put(&self, key: &str, bytes: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
        let key = key.to_owned();
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            Self::key_path(&self.primary_root, &key)?;
            Self::key_path(&self.replica_root, &key)?;
            self.admission(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            self.primary.put(&key, bytes).await?;
            self.sync_primary(&key).await?;
            self.copy_between(&key, &self.primary_root, &self.replica_root)
                .await
        })
    }
    fn put_file(&self, key: &str, source: &Path) -> BoxFuture<'_, std::io::Result<()>> {
        let key = key.to_owned();
        let source = source.to_path_buf();
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            Self::key_path(&self.primary_root, &key)?;
            Self::key_path(&self.replica_root, &key)?;
            self.admission(tokio::fs::metadata(&source).await?.len())?;
            self.primary.put_file(&key, &source).await?;
            self.sync_primary(&key).await?;
            self.copy_between(&key, &self.primary_root, &self.replica_root)
                .await
        })
    }
    fn append_synced(
        &self,
        key: &str,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, std::io::Result<()>> {
        let key = key.to_owned();
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            Self::key_path(&self.primary_root, &key)?;
            Self::key_path(&self.replica_root, &key)?;
            self.admission(u64::try_from(bytes.len()).unwrap_or(u64::MAX))?;
            for (from, to) in [
                (&self.primary_root, &self.replica_root),
                (&self.replica_root, &self.primary_root),
            ] {
                let path = Self::key_path(to, &key)?;
                let needs_prefix = match tokio::fs::metadata(path).await {
                    Ok(metadata) => metadata.len() < offset,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => offset != 0,
                    Err(error) => return Err(error),
                };
                if needs_prefix {
                    self.copy_between(&key, from, to).await?;
                }
            }
            let began = tokio::time::Instant::now();
            let count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            self.primary
                .append_synced(&key, offset, bytes.clone())
                .await?;
            if available_bytes(&self.replica_root)? < self.min_free.saturating_add(count) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "mirror append free-space floor",
                ));
            }
            self.replica.append_synced(&key, offset, bytes).await?;
            self.pace(began, count).await;
            Ok(())
        })
    }
    fn open(&self, key: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
        let key = key.to_owned();
        Box::pin(async move {
            Self::key_path(&self.primary_root, &key)?;
            let primary = self.check_root(&self.primary_root);
            if primary.is_ok()
                && let Ok(file) = self.primary.open(&key).await
            {
                return Ok(file);
            }
            self.check_root(&self.replica_root)?;
            Self::key_path(&self.replica_root, &key)?;
            match self.replica.open(&key).await {
                Ok(file) => Ok(file),
                Err(error) => Err(error),
            }
        })
    }
    fn get(&self, key: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
        let key = key.to_owned();
        Box::pin(async move {
            let mut opened = self.open(&key).await?;
            let mut bytes = Vec::new();
            opened.reader.read_to_end(&mut bytes).await?;
            Ok(bytes)
        })
    }
    fn local_path(&self, key: &str) -> Option<PathBuf> {
        for root in [&self.primary_root, &self.replica_root] {
            if self.check_root(root).is_err() {
                continue;
            }
            let path = Self::key_path(root, key).ok()?;
            if std::fs::symlink_metadata(&path)
                .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
            {
                return Some(path);
            }
        }
        None
    }
    fn delete(&self, key: &str) -> BoxFuture<'_, std::io::Result<()>> {
        let key = key.to_owned();
        Box::pin(async move {
            let _guard = self.writer.lock().await;
            self.check_root(&self.primary_root)?;
            self.check_root(&self.replica_root)?;
            Self::key_path(&self.primary_root, &key)?;
            let (primary, replica) =
                tokio::join!(self.primary.delete(&key), self.replica.delete(&key));
            primary.and(replica)?;
            Self::sync_root(self.primary_root.clone()).await?;
            Self::sync_root(self.replica_root.clone()).await
        })
    }
    fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
        Box::pin(async move {
            self.check_root(&self.primary_root)?;
            self.check_root(&self.replica_root)?;
            let mut keys = self.primary.list().await?;
            keys.extend(self.replica.list().await?);
            keys.retain(|key| !key.starts_with(".plamenu-"));
            keys.sort_unstable();
            keys.dedup();
            Ok(keys)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn writes_fall_back_without_buffering_and_delete_both_copies() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let store = Arc::new(
            MirroredDiskStore::new(
                primary.path().into(),
                replica.path().into(),
                64 * 1024 * 1024,
                0,
            )
            .unwrap(),
        );
        store.put("movie.mp4", vec![7; 128 * 1024]).await.unwrap();
        assert_eq!(
            tokio::fs::read(replica.path().join("movie.mp4"))
                .await
                .unwrap(),
            vec![7; 128 * 1024]
        );
        tokio::fs::remove_file(primary.path().join("movie.mp4"))
            .await
            .unwrap();
        assert_eq!(store.open("movie.mp4").await.unwrap().len, 128 * 1024);
        assert_eq!(store.get("movie.mp4").await.unwrap(), vec![7; 128 * 1024]);
        store.delete("movie.mp4").await.unwrap();
        assert!(store.open("movie.mp4").await.is_err());
    }
    #[tokio::test]
    async fn committed_chunks_retry_identically_on_both_disks() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let store = MirroredDiskStore::new(
            primary.path().into(),
            replica.path().into(),
            64 * 1024 * 1024,
            0,
        )
        .unwrap();
        store
            .append_synced("source.orig", 0, vec![1, 2])
            .await
            .unwrap();
        store
            .append_synced("source.orig", 2, vec![3, 4])
            .await
            .unwrap();
        store
            .append_synced("source.orig", 2, vec![5, 6])
            .await
            .unwrap();
        for root in [primary.path(), replica.path()] {
            assert_eq!(
                tokio::fs::read(root.join("source.orig")).await.unwrap(),
                vec![1, 2, 5, 6]
            );
        }
        assert!(store.put("../escape", vec![1]).await.is_err());
    }

    fn store(primary: &Path, replica: &Path) -> MirroredDiskStore {
        MirroredDiskStore::new(primary.into(), replica.into(), 64 * 1024 * 1024, 0).unwrap()
    }
    #[tokio::test]
    async fn existing_files_seed_and_corrupt_same_size_replica_is_repaired() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        tokio::fs::write(primary.path().join("movie.mp4"), [1, 2, 3])
            .await
            .unwrap();
        let mirror = store(primary.path(), replica.path());
        assert_eq!(mirror.seed().await.unwrap(), 1);
        assert_eq!(mirror.seed().await.unwrap(), 0);
        tokio::fs::write(replica.path().join("movie.mp4"), [3, 2, 1])
            .await
            .unwrap();
        assert_eq!(mirror.seed().await.unwrap(), 1);
        assert_eq!(
            tokio::fs::read(replica.path().join("movie.mp4"))
                .await
                .unwrap(),
            [1, 2, 3]
        );
        assert_eq!(mirror.list().await.unwrap(), ["movie.mp4"]);
        drop(mirror);
        assert_eq!(
            store(primary.path(), replica.path()).seed().await.unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn missing_mount_refuses_writes_and_deletion_ack_but_retains_read_failover() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror = store(primary.path(), replica.path());
        mirror.put("movie.mp4", vec![1, 2, 3]).await.unwrap();
        let marker = tokio::fs::read(primary.path().join(MARKER)).await.unwrap();
        tokio::fs::remove_file(primary.path().join(MARKER))
            .await
            .unwrap();
        assert_eq!(mirror.get("movie.mp4").await.unwrap(), [1, 2, 3]);
        assert!(mirror.put("new.mp4", vec![9]).await.is_err());
        assert!(mirror.delete("movie.mp4").await.is_err());
        assert!(replica.path().join("movie.mp4").exists());
        assert!(
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 1, 0).is_err()
        );
        tokio::fs::write(primary.path().join(MARKER), marker)
            .await
            .unwrap();
        mirror.delete("movie.mp4").await.unwrap();
        assert!(mirror.open("movie.mp4").await.is_err());
    }
    #[tokio::test]
    async fn incomplete_mirror_delete_is_retried_without_losing_the_other_copy() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror = store(primary.path(), replica.path());
        mirror.put("movie.mp4", vec![1, 2, 3]).await.unwrap();
        tokio::fs::remove_file(replica.path().join("movie.mp4"))
            .await
            .unwrap();
        tokio::fs::create_dir(replica.path().join("movie.mp4"))
            .await
            .unwrap();
        assert!(mirror.delete("movie.mp4").await.is_err());
        assert!(replica.path().join("movie.mp4").is_dir());
        tokio::fs::remove_dir(replica.path().join("movie.mp4"))
            .await
            .unwrap();
        mirror.delete("movie.mp4").await.unwrap();
    }
    #[tokio::test]
    async fn missing_upload_prefix_recovers_from_surviving_disk_before_append() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror = store(primary.path(), replica.path());
        mirror
            .append_synced("source.orig", 0, vec![1, 2])
            .await
            .unwrap();
        tokio::fs::remove_file(primary.path().join("source.orig"))
            .await
            .unwrap();
        mirror
            .append_synced("source.orig", 2, vec![3, 4])
            .await
            .unwrap();
        tokio::fs::remove_file(replica.path().join("source.orig"))
            .await
            .unwrap();
        mirror
            .append_synced("source.orig", 4, vec![5, 6])
            .await
            .unwrap();
        tokio::fs::write(primary.path().join("source.orig"), [9])
            .await
            .unwrap();
        mirror
            .append_synced("source.orig", 6, vec![7, 8])
            .await
            .unwrap();
        for root in [primary.path(), replica.path()] {
            assert_eq!(
                tokio::fs::read(root.join("source.orig")).await.unwrap(),
                [1, 2, 3, 4, 5, 6, 7, 8]
            );
        }
    }
    #[tokio::test]
    async fn admission_and_serial_bandwidth_budget_cover_put_file_and_append() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror =
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 64 * 1024, 0)
                .unwrap();
        let began = tokio::time::Instant::now();
        let source = primary.path().join("upload.tmp");
        tokio::fs::write(&source, vec![7; 16 * 1024]).await.unwrap();
        mirror.put_file("movie.mp4", &source).await.unwrap();
        mirror
            .append_synced("source.orig", 0, vec![4; 16 * 1024])
            .await
            .unwrap();
        assert!(began.elapsed() >= Duration::from_millis(490));
        assert!(!source.exists());
        let full =
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 1, u64::MAX)
                .unwrap();
        assert_eq!(
            full.put("refused.mp4", vec![1]).await.unwrap_err().kind(),
            std::io::ErrorKind::StorageFull
        );
        assert!(!primary.path().join("refused.mp4").exists());
    }
    #[test]
    fn rejects_unowned_nonempty_nested_missing_and_zero_budget_replicas() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        std::fs::write(replica.path().join("operator-notes"), b"preserve").unwrap();
        assert!(
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 1, 0).is_err()
        );
        assert!(
            MirroredDiskStore::new(primary.path().into(), primary.path().into(), 1, 0).is_err()
        );
        assert!(
            MirroredDiskStore::new(primary.path().into(), primary.path().join("missing"), 1, 0)
                .is_err()
        );
        assert!(
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 0, 0).is_err()
        );
        assert_eq!(
            std::fs::read(replica.path().join("operator-notes")).unwrap(),
            b"preserve"
        );
    }
    #[tokio::test]
    async fn crash_staging_cleanup_preserves_recent_and_unrelated_files() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror = store(primary.path(), replica.path());
        let staging = replica.path().join(STAGING);
        std::fs::create_dir(&staging).unwrap();
        let old = staging.join(format!("{}.tmp", crate::media_processing::storage_stem(0)));
        let recent = staging.join(format!("{}.tmp", crate::media_processing::storage_stem(0)));
        let unrelated = staging.join("operator-notes");
        for path in [&old, &recent, &unrelated] {
            std::fs::write(path, b"keep unless owned and stale").unwrap();
        }
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_hours(49))
            .unwrap();
        mirror.seed().await.unwrap();
        assert!(!old.exists());
        assert!(recent.exists() && unrelated.exists());
    }
    #[tokio::test]
    async fn cancelling_a_copy_removes_only_its_owned_temporary_file() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let mirror = Arc::new(
            MirroredDiskStore::new(primary.path().into(), replica.path().into(), 64 * 1024, 0)
                .unwrap(),
        );
        let copy = {
            let mirror = mirror.clone();
            tokio::spawn(async move { mirror.put("movie.mp4", vec![7; 128 * 1024]).await })
        };
        let staging = replica.path().join(STAGING);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(mut entries) = tokio::fs::read_dir(&staging).await
                    && entries.next_entry().await.unwrap().is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        copy.abort();
        assert!(copy.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut entries = tokio::fs::read_dir(&staging).await.unwrap();
                if entries.next_entry().await.unwrap().is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!replica.path().join("movie.mp4").exists());
        assert_eq!(
            tokio::fs::metadata(primary.path().join("movie.mp4"))
                .await
                .unwrap()
                .len(),
            128 * 1024
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn never_follows_key_or_staging_symlinks() {
        let primary = tempfile::tempdir().unwrap();
        let replica = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mirror = store(primary.path(), replica.path());
        std::fs::write(outside.path().join("secret"), b"private").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            primary.path().join("leak.mp4"),
        )
        .unwrap();
        assert!(mirror.open("leak.mp4").await.is_err());
        assert!(mirror.put("leak.mp4", vec![9]).await.is_err());
        std::os::unix::fs::symlink(outside.path(), replica.path().join(STAGING)).unwrap();
        assert!(mirror.put("movie.mp4", vec![9]).await.is_err());
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"private"
        );
    }
}
