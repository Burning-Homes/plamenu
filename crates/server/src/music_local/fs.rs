//! Directory-relative file opens reject traversal, symlinks and special files.
//! A held descriptor gives subprocesses a stable original without copying it.
mod mutation;
pub use mutation::Staged;
use rustix::fs::{Mode, OFlags, openat};
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
use std::{
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
}
impl Fingerprint {
    fn of(file: &File) -> io::Result<Self> {
        let info = file.metadata()?;
        Ok(Self {
            device: info.dev(),
            inode: info.ino(),
            bytes: info.len(),
            modified_seconds: info.mtime(),
            modified_nanos: info.mtime_nsec(),
        })
    }
}
pub struct Root {
    directory: File,
}
pub struct Opened {
    file: File,
    pub fingerprint: Fingerprint,
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Invalid music folder reference",
    )
}
pub fn components(relative: &str) -> io::Result<Vec<&std::ffi::OsStr>> {
    if relative.is_empty()
        || relative.len() > 2048
        || relative.contains('\0')
        || relative
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(invalid());
    }
    Path::new(relative)
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => Err(invalid()),
        })
        .collect()
}
impl Root {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid());
        }
        // The root is selected exclusively by operator configuration. Children
        // are opened from this held directory, rather than resolving new paths.
        let directory: File = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        if !directory.metadata()?.is_dir() {
            return Err(invalid());
        }
        Ok(Self { directory })
    }
    pub fn identity(&self) -> io::Result<(u64, u64)> {
        let info = self.directory.metadata()?;
        Ok((info.dev(), info.ino()))
    }
    pub fn check_directory(&self, relative: &str) -> io::Result<()> {
        self.directory(relative).map(|_| ())
    }
    fn directory(&self, relative: &str) -> io::Result<File> {
        let mut directory = self.directory.try_clone()?;
        if !relative.is_empty() {
            for part in components(relative)? {
                directory = openat(
                    &directory,
                    part,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?
                .into();
            }
        }
        Ok(directory)
    }
    /// Enumerate from held directory descriptors; a replaced parent cannot
    /// redirect traversal. Abort incomplete scans instead of declaring files gone.
    pub fn scan(&self, relative: &str) -> io::Result<Vec<(String, Fingerprint)>> {
        self.scan_with_options(relative, &super::options::Options::default())
    }
    pub fn scan_with_options(
        &self,
        relative: &str,
        options: &super::options::Options,
    ) -> io::Result<Vec<(String, Fingerprint)>> {
        let selection = options.selection().map_err(|_| invalid())?;
        let mut stack = vec![(relative.to_owned(), self.directory(relative)?)];
        let mut records = Vec::new();
        let mut directories = 0;
        while let Some((prefix, directory)) = stack.pop() {
            directories += 1;
            if directories > 10000 {
                return Err(io::Error::other("Music folder directory limit exceeded"));
            }
            let held = format!("/proc/{}/fd/{}", std::process::id(), directory.as_raw_fd());
            for entry in std::fs::read_dir(held)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if name.starts_with(".plamenu-") {
                    continue;
                }
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                components(&path)?;
                if selection.excludes(&path) {
                    continue;
                }
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    if !selection.recursive {
                        continue;
                    }
                    let child: File = openat(
                        &directory,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?
                    .into();
                    stack.push((path, child));
                } else if kind.is_file() && selection.audio(&name) {
                    let file: File = openat(
                        &directory,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?
                    .into();
                    if !file.metadata()?.is_file() {
                        continue;
                    }
                    let fingerprint = Fingerprint::of(&file)?;
                    if fingerprint.bytes > 0 {
                        records.push((path, fingerprint));
                    }
                    if records.len() > 10000 {
                        return Err(io::Error::other("Music folder track limit exceeded"));
                    }
                }
            }
        }
        records.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        Ok(records)
    }
    pub fn file(&self, relative: &str) -> io::Result<Opened> {
        let parts = components(relative)?;
        let (leaf, parents) = parts.split_last().ok_or_else(invalid)?;
        let mut directory = self.directory.try_clone()?;
        for parent in parents {
            directory = openat(
                &directory,
                *parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into();
        }
        let file: File = openat(
            &directory,
            *leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        if !file.metadata()?.is_file() {
            return Err(invalid());
        }
        let fingerprint = Fingerprint::of(&file)?;
        Ok(Opened { file, fingerprint })
    }
}
impl Opened {
    /// The descriptor is kept alive while the caller awaits its subprocess.
    #[must_use]
    pub fn process_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }
    #[must_use]
    pub fn into_reader(self) -> crate::storage::MediaRead {
        crate::storage::MediaRead {
            len: self.fingerprint.bytes,
            reader: Box::new(tokio::fs::File::from_std(self.file)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;
    #[test]
    fn rejects_traversal_parent_or_leaf_symlinks_and_special_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("album")).unwrap();
        std::fs::write(root.path().join("album/song.wav"), b"audio").unwrap();
        std::fs::write(outside.path().join("private.wav"), b"private").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        symlink(
            outside.path().join("private.wav"),
            root.path().join("album/link.wav"),
        )
        .unwrap();
        let opened = Root::open(root.path()).unwrap();
        for relative in [
            "",
            "../private.wav",
            "/etc/passwd",
            "album/../song.wav",
            "linked/private.wav",
            "album/link.wav",
            "album",
            "album/./song.wav",
        ] {
            assert!(opened.file(relative).is_err(), "{relative}");
        }
        let file = opened.file("album/song.wav").unwrap();
        assert_eq!(file.fingerprint.bytes, 5);
    }
    #[test]
    fn configured_and_child_fifos_are_rejected_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("recording.wav");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let root = Root::open(directory.path()).unwrap();
        assert!(root.file("recording.wav").is_err());
        let (sender, receiver) = std::sync::mpsc::channel();
        let target = fifo.clone();
        let worker = std::thread::spawn(move || {
            sender.send(Root::open(&target).is_err()).unwrap();
        });
        let result = receiver.recv_timeout(std::time::Duration::from_millis(500));
        if result.is_err() {
            // Unblock a regressed blocking open so a failure leaves no thread.
            let _writer = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo)
                .unwrap();
            worker.join().unwrap();
            panic!("Opening a configured FIFO blocked");
        }
        assert!(result.unwrap());
        worker.join().unwrap();
    }
    #[test]
    fn held_original_is_seekable_and_survives_directory_replacement_without_copying() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("album")).unwrap();
        std::fs::write(root.path().join("album/song.wav"), b"original").unwrap();
        let opened = Root::open(root.path())
            .unwrap()
            .file("album/song.wav")
            .unwrap();
        let path = opened.process_path();
        std::fs::rename(root.path().join("album"), root.path().join("old-album")).unwrap();
        std::fs::create_dir(root.path().join("album")).unwrap();
        std::fs::write(root.path().join("album/song.wav"), b"replacement").unwrap();
        let mut bytes = Vec::new();
        File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert_eq!(
            std::fs::metadata(root.path().join("old-album/song.wav"))
                .unwrap()
                .nlink(),
            1
        );
    }
}
