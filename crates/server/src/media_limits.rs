//! Per-encoder kernel file limits without unsafe pre-exec hooks or an external
//! helper installation. The server launches its own hidden CLI entry point,
//! which sets limits and execs `FFmpeg` in the same process. Cancellation therefore
//! still kills the actual encoder. No limits are changed in the server process.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

/// Re-exec the *running* binary even while a package upgrade atomically
/// replaces its pathname. Linux keeps the old executable reachable through
/// procfs until the process exits; `current_exe()` may instead return a path
/// ending in ` (deleted)`, which cannot launch a bounded media job. This also
/// lets a long-running worker finish work during an atomic binary upgrade.
pub(crate) fn running_executable() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    if Path::new("/proc/self/exe").exists() {
        return Ok(PathBuf::from("/proc/self/exe"));
    }
    std::env::current_exe()
}

/// A per-file hard ceiling, not a total scratch/storage quota. The caller must
/// reserve for every output/intermediate and bound concurrent workers as well.
#[derive(Clone, Debug)]
pub struct FileSizeLimit {
    /// The installed Plamenu executable; tests pass their built CLI artifact.
    pub launcher: std::path::PathBuf,
    pub bytes: u64,
}

/// Internal CLI only: called before starting Tokio, loading configuration or
/// opening a database. On success exec never returns. Unsupported hosts refuse
/// bounded jobs instead of silently running an unbounded encoder.
#[cfg(unix)]
#[must_use]
pub fn exec(ffmpeg: &Path, args: &[OsString], bytes: u64, parent_pid: u32) -> io::Error {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    use std::os::unix::process::CommandExt;

    // kill_on_drop covers orderly cancellation, but cannot run if the server
    // is killed. Ask the kernel to kill the encoder on parent death too. The
    // following parent check closes the spawn → prctl race.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if let Err(error) =
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
    {
        return error.into();
    }
    if rustix::process::getppid().and_then(|pid| u32::try_from(pid.as_raw_pid()).ok())
        != Some(parent_pid)
    {
        return io::Error::new(
            io::ErrorKind::Interrupted,
            "encoder parent exited before launch",
        );
    }

    if bytes == 0 {
        return io::Error::new(io::ErrorKind::InvalidInput, "zero encoder file limit");
    }
    let inherited = getrlimit(Resource::Fsize);
    let ceiling = bytes
        .min(inherited.current.unwrap_or(u64::MAX))
        .min(inherited.maximum.unwrap_or(u64::MAX));
    if let Err(error) = setrlimit(
        Resource::Fsize,
        Rlimit {
            current: Some(ceiling),
            maximum: Some(ceiling),
        },
    ) {
        return error.into();
    }
    // SIGXFSZ can otherwise leave a large core dump after hitting the disk cap.
    if let Err(error) = setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    ) {
        return error.into();
    }
    std::process::Command::new(ffmpeg).args(args).exec()
}

#[cfg(not(unix))]
#[must_use]
pub fn exec(_ffmpeg: &Path, _args: &[OsString], _bytes: u64, _parent_pid: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded native encoding requires kernel file limits",
    )
}

/// Only the child gets the limit. Re-exec replaces the child, so `kill_on_drop`
/// cannot strand a helper's encoder grandchild after a lost lease.
pub(crate) fn command(ffmpeg: &Path, limit: Option<&FileSizeLimit>) -> tokio::process::Command {
    if let Some(limit) = limit {
        let mut command = tokio::process::Command::new(&limit.launcher);
        command
            .arg("__media-exec")
            .arg("--max-file-bytes")
            .arg(limit.bytes.to_string())
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .arg("--ffmpeg")
            .arg(ffmpeg)
            .arg("--");
        command
    } else {
        tokio::process::Command::new(ffmpeg)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn running_binary_can_be_reexecuted() {
        let launcher = super::running_executable().unwrap();
        assert!(
            std::process::Command::new(launcher)
                .arg("--help")
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}
