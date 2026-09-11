"""Image/media helpers shared across the peer federation tests.

`make_png`/`tiny_png` build valid PNGs by hand (no third-party imaging
library), and `cached_attachment` polls a Plamenu status for its first
attachment once Plamenu has proxied/cached the file onto its own `/media/`
route. The oldest media tests (`tests/test_media.py`, `tests/test_profile.py`)
keep their own local copies; new peer modules import these.
"""

import struct
import subprocess
import tempfile
import zlib
from pathlib import Path

from . import config


def _chunk(kind: bytes, payload: bytes) -> bytes:
    return (
        struct.pack(">I", len(payload))
        + kind
        + payload
        + struct.pack(">I", zlib.crc32(kind + payload))
    )


def make_png(width: int = 64, height: int = 48, rgb=(200, 100, 50)) -> bytes:
    """A minimal valid RGB PNG of the given size."""
    raw = b"".join(b"\x00" + bytes(rgb) * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + _chunk(b"IDAT", zlib.compress(raw))
        + _chunk(b"IEND", b"")
    )


def tiny_png(rgb=(200, 30, 30)) -> bytes:
    """A 1x1 PNG — small enough for every avatar/emoji size limit."""
    return make_png(1, 1, rgb)


def make_avif(width: int = 64, height: int = 48, rgb=(200, 100, 50)) -> bytes:
    """A still AVIF encoded by the same required FFmpeg/libaom stack as
    Plamenu. A real file (rather than a hand-built container) exercises both
    Lemmy/pict-rs and Plamenu's decoder."""
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / "source.png"
        output = Path(tmp) / "image.avif"
        source.write_bytes(make_png(width, height, rgb))
        subprocess.run(
            [
                "ffmpeg",
                "-nostdin",
                "-loglevel",
                "fatal",
                "-i",
                str(source),
                "-frames:v",
                "1",
                "-c:v",
                "libaom-av1",
                "-still-picture",
                "1",
                "-cpu-used",
                "8",
                "-crf",
                "30",
                "-b:v",
                "0",
                "-pix_fmt",
                "yuv444p",
                "-y",
                str(output),
            ],
            check=True,
        )
        return output.read_bytes()


def cached_attachment(
    plamenu_api, status_id, *, kind=None, require_blurhash=False, base_url=None
):
    """The status' first attachment once Plamenu has cached it locally (its
    `url` points at our own `/media/` route), else None — for `wait_for`.

    The `/media/` url flips as soon as the file is proxied, but Plamenu computes
    the blurhash + `meta` dimensions asynchronously and backfills them a moment
    later. Pass `require_blurhash=True` so callers that assert blurhash/dimensions
    wait for that backfill instead of racing it (the two land together).

    `base_url` names the instance doing the caching; it defaults to the dev
    server, and the Plamenu-against-Plamenu tests pass the peer's URL."""
    atts = plamenu_api.get_status(str(status_id)).get("media_attachments") or []
    if not atts:
        return None
    att = atts[0]
    local = (att.get("url") or "").startswith(
        f"{base_url or config.PLAMENU_URL}/media/"
    )
    if not (local and (kind is None or att["type"] == kind)):
        return None
    if require_blurhash and not (att.get("blurhash") and att.get("meta")):
        return None
    return att
