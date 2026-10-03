"""Select the same executable for CLI, disposable peers and crash recovery."""

import os
import subprocess
from pathlib import Path

from . import config


def binary_path() -> Path:
    if config.PLAMENU_BINARY:
        path = Path(config.PLAMENU_BINARY)
        if not path.is_absolute():
            raise ValueError("PLAMENU_BINARY must be an absolute executable path")
        return path.resolve()
    return (config.PLAMENU_DIR / "target/debug/plamenu").resolve()


def prepare_binary() -> Path:
    if not config.PLAMENU_BINARY:
        subprocess.run(
            ["cargo", "build", "-q"], cwd=config.PLAMENU_DIR, check=True, timeout=1800
        )
    path = binary_path()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"Plamenu executable is missing or not executable: {path}")
    return path


def exact_local_server(pid: int) -> Path:
    """Require the explicit candidate, config, cwd and executable before a kill."""
    assert config.PLAMENU_BINARY, "Crash drills require explicit PLAMENU_BINARY"
    root = config.PLAMENU_DIR.resolve()
    binary = binary_path()
    assert binary.is_file(), "Configured candidate executable is missing"
    cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
    args = [part.decode() for part in cmdline if part]
    assert len(args) == 4 and args[-3:] == [
        "--config",
        config.PLAMENU_CONFIG,
        "serve",
    ], "Server command line differs from the disposable fixture"
    assert Path(f"/proc/{pid}/cwd").resolve() == root
    invoked = Path(args[0])
    assert (invoked if invoked.is_absolute() else root / invoked).resolve() == binary
    assert Path(f"/proc/{pid}/exe").resolve() == binary
    return binary
