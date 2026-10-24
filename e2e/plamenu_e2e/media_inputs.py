"""Bounded reuse of immutable source clips; every caller owns a writable copy."""

import shutil
import subprocess
import tempfile
from collections import OrderedDict
from pathlib import Path


def _identity(encoder):
    stat = encoder.stat()
    return (
        str(encoder),
        stat.st_dev,
        stat.st_ino,
        stat.st_size,
        stat.st_mtime_ns,
        stat.st_ctime_ns,
        stat.st_mode,
    )


class SampleVideoInputs:
    """A process-local LRU; no persistent files or shared writable fixtures.

    The encoder must still exist on every request. Its resolved path and file
    version, every encoding argument, and the output container bind each entry.
    A different/replaced encoder must generate its own input. The byte and entry
    limits also bound a long run with many clip sizes or durations.
    """

    def __init__(self, *, max_entries=16, max_bytes=16 * 1024 * 1024):
        self.max_entries = max_entries
        self.max_bytes = max_bytes
        self._entries = OrderedDict()
        self._bytes = 0

    def make_mp4(self, path, seconds=5, size="320x240"):
        path = Path(path)
        resolved = shutil.which("ffmpeg")
        if resolved is None:
            raise FileNotFoundError(
                "ffmpeg is required to generate sample video inputs"
            )
        encoder = Path(resolved).resolve(strict=True)
        identity = _identity(encoder)
        command = (
            str(encoder),
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            f"testsrc=duration={seconds}:size={size}:rate=30",
            "-f",
            "lavfi",
            "-i",
            f"sine=frequency=440:duration={seconds}",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            "-y",
        )
        # Keep FFmpeg's format inference and failure behavior for other suffixes.
        if path.suffix.lower() != ".mp4":
            subprocess.run([*command, str(path)], check=True)
            return path
        key = (identity, command, ".mp4")
        content = self._entries.pop(key, None)
        if content is not None:
            self._entries[key] = content
        else:
            with tempfile.TemporaryDirectory(prefix="plamenu-sample-input-") as folder:
                generated = Path(folder) / "source.mp4"
                subprocess.run([*command, str(generated)], check=True)
                if _identity(encoder) != identity:
                    raise RuntimeError("sample video encoder changed during generation")
                length = generated.stat().st_size
                if not length:
                    raise RuntimeError("sample video encoder produced an empty input")
                if length <= self.max_bytes and self.max_entries > 0:
                    content = generated.read_bytes()
                    while self._entries and (
                        len(self._entries) >= self.max_entries
                        or self._bytes + length > self.max_bytes
                    ):
                        _, evicted = self._entries.popitem(last=False)
                        self._bytes -= len(evicted)
                    self._entries[key] = content
                    self._bytes += length
                else:
                    # Oversized inputs are copied without retaining them in memory.
                    shutil.copyfile(generated, path)
                    return path
        path.write_bytes(content)
        return path


_SAMPLE_VIDEOS = SampleVideoInputs()


def make_sample_mp4(path, seconds: int = 5, size: str = "320x240") -> Path:
    """Encode once per source/version in this process, then copy immutable bytes."""
    return _SAMPLE_VIDEOS.make_mp4(path, seconds, size)
