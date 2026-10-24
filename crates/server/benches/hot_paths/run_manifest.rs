//! A failed/interrupted attempt must supersede the previous completed run.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const MANIFEST: &str = "plamenu-bench-run.json";

pub struct RunManifest {
    directory: PathBuf,
    archive: PathBuf,
    metadata: Value,
}

impl RunManifest {
    /// Publish a nonpassing boundary before argument validation or fixture setup.
    /// Completed manifests also live in unique directories; replacing the latest
    /// boundary never destroys earlier evidence.
    pub fn begin(directory: &Path, mut metadata: Value) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let previous = match std::fs::read(directory.join(MANIFEST)) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        // Invalidate the previous pointer first. If creating/archiving the new
        // attempt fails, absence must not look like the previous run succeeded.
        match std::fs::remove_file(directory.join(MANIFEST)) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let runs = directory.join("plamenu-runs");
        std::fs::create_dir_all(&runs)?;
        if let Some(previous) = previous {
            let mut file = tempfile::Builder::new()
                .prefix("previous-")
                .suffix(".json")
                .tempfile_in(&runs)?;
            file.write_all(&previous)?;
            file.as_file().sync_all()?;
            file.keep()?;
        }
        let archive = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(runs)?
            .keep();
        let id = archive
            .file_name()
            .expect("unique run directory")
            .to_string_lossy();
        metadata["run_schema"] = json!(1);
        metadata["run_id"] = json!(id);
        metadata["run_status"] = json!("running");
        let run = Self {
            directory: directory.to_owned(),
            archive,
            metadata,
        };
        Self::write(&run.archive.join(MANIFEST), &run.metadata)?;
        Self::write(&run.directory.join(MANIFEST), &run.metadata)?;
        Ok(run)
    }

    pub fn finish(self, extra: Value) -> io::Result<()> {
        let latest: Value = serde_json::from_slice(&std::fs::read(self.directory.join(MANIFEST))?)?;
        if latest["run_id"] != self.metadata["run_id"] || latest["run_status"] != "running" {
            return Err(io::Error::other("benchmark run boundary was replaced"));
        }
        let Value::Object(extra) = extra else {
            return Err(io::Error::other("completed manifest must be an object"));
        };
        let mut completed = self.metadata;
        completed
            .as_object_mut()
            .expect("manifest object")
            .extend(extra);
        completed["run_status"] = json!("complete");
        // Archive before publishing completion. A failed write leaves the
        // latest boundary running, and the caller must exit unsuccessfully.
        Self::write(&self.archive.join(MANIFEST), &completed)?;
        Self::write(&self.directory.join(MANIFEST), &completed)
    }

    fn write(path: &Path, value: &Value) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(path.parent().expect("manifest parent"))?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}

/// Bind the driver and Criterion to the same directory, including custom homes.
pub fn output_directory(
    home: Option<OsString>,
    target: Option<OsString>,
    manifest: &Path,
) -> PathBuf {
    if let Some(home) = home {
        return home.into();
    }
    if let Some(target) = target {
        return PathBuf::from(target).join("criterion");
    }
    manifest
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .map_or_else(
            || PathBuf::from("target/criterion"),
            |root| root.join("target/criterion"),
        )
}
