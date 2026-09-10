"""Public-status and RTMP helpers for the source-built Owncast peer."""

import contextlib
import subprocess
import time

import requests
import urllib3

from . import config

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

ACTOR = f"streamer@{config.OWNCAST_DOMAIN}"
STREAM_KEY = "owncast-stream-key-123"
RTMP_TARGET = f"rtmp://127.0.0.1:1937/live/{STREAM_KEY}"


def status() -> dict:
    response = requests.get(
        f"{config.OWNCAST_URL}/api/status", verify=False, timeout=10
    )
    response.raise_for_status()
    return response.json()


def reachable() -> bool:
    try:
        return "online" in status()
    except (requests.RequestException, ValueError):
        return False


def wait_online(online: bool, *, timeout: float = 45.0) -> dict:
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = status()
            if bool(last.get("online")) is online:
                return last
        except (requests.RequestException, ValueError) as error:
            last = {"error": str(error)}
        time.sleep(1)
    raise TimeoutError(f"Owncast online={online} not reached; last status={last!r}")


@contextlib.contextmanager
def broadcast(*, size: str = "640x360", rate: int = 30):
    """Push a synthetic H.264/AAC live stream until the context exits."""
    proc = subprocess.Popen(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            f"testsrc2=size={size}:rate={rate}",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=523",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-tune",
            "zerolatency",
            "-g",
            str(rate * 2),
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "flv",
            RTMP_TARGET,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    try:
        try:
            wait_online(True)
        except Exception:
            if proc.poll() is not None:
                error = (proc.stderr.read() or b"").decode(errors="replace")
                raise RuntimeError(f"Owncast RTMP push died: {error[:500]}") from None
            raise
        yield proc
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
        wait_online(False)
