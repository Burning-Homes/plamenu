//! Mutations use the same held parent directory and never overwrite a target.
use super::{Fingerprint, Opened, Root, components, invalid};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, openat, renameat_with, unlinkat};
use std::{fs::File, io, path::Path};
fn open(directory: &File, name: &str) -> io::Result<Opened> {
    let file: File = openat(
        directory,
        name,
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
fn parent(relative: &str) -> io::Result<(&str, &str)> {
    components(relative)?;
    Ok(relative.rsplit_once('/').unwrap_or(("", relative)))
}
fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "Music original changed; rescan before editing",
    )
}
impl Root {
    pub fn vacant(&self, relative: &str) -> io::Result<bool> {
        let (prefix, name) = parent(relative)?;
        let directory = self.directory(prefix)?;
        match rustix::fs::statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(false),
            Err(rustix::io::Errno::NOENT) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }
    pub fn parent_identity(&self, relative: &str) -> io::Result<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let (prefix, _) = parent(relative)?;
        let info = self.directory(prefix)?.metadata()?;
        Ok((info.dev(), info.ino()))
    }
    /// The replacement name retains the old inode until the database commits.
    pub fn exchange_files(
        &self,
        original: &str,
        replacement: &str,
        before: Fingerprint,
        after: Fingerprint,
    ) -> io::Result<()> {
        let (prefix, leaf) = parent(original)?;
        let (other, name) = parent(replacement)?;
        if prefix != other {
            return Err(invalid());
        }
        let directory = self.directory(prefix)?;
        if open(&directory, leaf)?.fingerprint != before
            || open(&directory, name)?.fingerprint != after
        {
            return Err(changed());
        }
        renameat_with(&directory, leaf, &directory, name, RenameFlags::EXCHANGE)?;
        if open(&directory, name)?.fingerprint != before
            || open(&directory, leaf)?.fingerprint != after
        {
            renameat_with(&directory, leaf, &directory, name, RenameFlags::EXCHANGE)?;
            return Err(changed());
        }
        directory.sync_all()?;
        Ok(())
    }
    pub fn rename_file(
        &self,
        relative: &str,
        name: &str,
        expected: Fingerprint,
    ) -> io::Result<String> {
        if name.is_empty()
            || name.len() > 255
            || name.contains(['/', '\\', '\0'])
            || matches!(name, "." | "..")
        {
            return Err(invalid());
        }
        let (prefix, old) = parent(relative)?;
        let directory = self.directory(prefix)?;
        if open(&directory, old)?.fingerprint != expected {
            return Err(changed());
        }
        if old == name {
            return Ok(relative.into());
        }
        renameat_with(&directory, old, &directory, name, RenameFlags::NOREPLACE)?;
        if open(&directory, name)?.fingerprint != expected {
            // A concurrently replaced source is restored without overwriting a
            // new file the operator may have placed at the old name.
            let _ = renameat_with(&directory, name, &directory, old, RenameFlags::NOREPLACE);
            return Err(changed());
        }
        directory.sync_all()?;
        Ok(if prefix.is_empty() {
            name.into()
        } else {
            format!("{prefix}/{name}")
        })
    }
    pub fn delete_file(&self, relative: &str, expected: Fingerprint) -> io::Result<()> {
        let (prefix, name) = parent(relative)?;
        let directory = self.directory(prefix)?;
        if open(&directory, name)?.fingerprint != expected {
            return Err(changed());
        }
        // Move first, verify the exact inode that moved, then unlink. A name
        // replacement between validation and rename cannot delete another file.
        let quarantine = format!(".plamenu-remove-{}", plamenu_db::id::next());
        renameat_with(
            &directory,
            name,
            &directory,
            quarantine.as_str(),
            RenameFlags::NOREPLACE,
        )?;
        if open(&directory, &quarantine)?.fingerprint != expected {
            let _ = renameat_with(
                &directory,
                quarantine.as_str(),
                &directory,
                name,
                RenameFlags::NOREPLACE,
            );
            return Err(changed());
        }
        unlinkat(&directory, quarantine.as_str(), AtFlags::empty())?;
        directory.sync_all()?;
        Ok(())
    }
    /// Tag remuxes are created privately beside the original so installation
    /// can exchange atomically and roll back a raced original without data loss.
    pub fn stage(&self, relative: &str) -> io::Result<Staged> {
        let (prefix, original) = parent(relative)?;
        let directory = self.directory(prefix)?;
        let extension = Path::new(original)
            .extension()
            .and_then(|v| v.to_str())
            .ok_or_else(invalid)?;
        let name = format!(".plamenu-tags-{}.{}", plamenu_db::id::next(), extension);
        let output: File = openat(
            &directory,
            name.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?
        .into();
        Ok(Staged {
            directory,
            original: original.into(),
            name,
            output,
            installed: false,
        })
    }
}
pub struct Staged {
    directory: File,
    original: String,
    name: String,
    output: File,
    installed: bool,
}
impl Staged {
    /// Commit a prepared, durable replacement to the mutation journal. Drop
    /// cleans it on every error before persistence; afterwards recovery owns it.
    pub fn retain(mut self, expected: Fingerprint) -> io::Result<(String, Fingerprint)> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let original = open(&self.directory, &self.original)?;
        if original.fingerprint != expected {
            return Err(changed());
        }
        self.output
            .set_permissions(std::fs::Permissions::from_mode(
                original.file.metadata()?.mode() & 0o777,
            ))?;
        self.output.sync_all()?;
        self.directory.sync_all()?;
        let fp = Fingerprint::of(&self.output)?;
        self.installed = true;
        Ok((self.name.clone(), fp))
    }
    #[must_use]
    pub fn process_path(&self) -> std::path::PathBuf {
        use std::os::fd::AsRawFd;
        format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.output.as_raw_fd()
        )
        .into()
    }
    pub fn install(mut self, expected: Fingerprint) -> io::Result<Fingerprint> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let original = open(&self.directory, &self.original)?;
        self.output
            .set_permissions(std::fs::Permissions::from_mode(
                original.file.metadata()?.mode() & 0o777,
            ))?;
        self.output.sync_all()?;
        if open(&self.directory, &self.original)?.fingerprint != expected {
            return Err(changed());
        }
        renameat_with(
            &self.directory,
            self.name.as_str(),
            &self.directory,
            self.original.as_str(),
            RenameFlags::EXCHANGE,
        )?;
        // From this point the staging name holds the original. Preserve it on
        // every error unless the exchange has been rolled back successfully.
        self.installed = true;
        if open(&self.directory, &self.name)?.fingerprint != expected {
            renameat_with(
                &self.directory,
                self.name.as_str(),
                &self.directory,
                self.original.as_str(),
                RenameFlags::EXCHANGE,
            )?;
            self.installed = false;
            return Err(changed());
        }
        self.installed = true;
        let fp = Fingerprint::of(&self.output)?;
        unlinkat(&self.directory, self.name.as_str(), AtFlags::empty())?;
        self.directory.sync_all()?;
        Ok(fp)
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.installed {
            let _ = unlinkat(&self.directory, self.name.as_str(), AtFlags::empty());
        }
    }
}
