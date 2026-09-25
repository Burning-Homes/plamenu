"""Accelerated long-window DVR and concurrent viewer pressure on native live."""

import hashlib
import os
import re
import shutil
import statistics
import subprocess
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, unique
from plamenu_e2e.api import Api
from plamenu_e2e.db import Db
from plamenu_e2e.steps import log, step, wait_for


def _get(url: str):
    return requests.get(url, verify=False, timeout=20)


def _playlist(media_id: str):
    master = _get(f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8")
    if master.status_code != 200:
        return None
    child = next((line for line in master.text.splitlines() if line.startswith("/media/")), None)
    if not child:
        return None
    response = _get(config.PLAMENU_URL + child)
    if response.status_code != 200:
        return None
    match = re.search(r"#EXT-X-MEDIA-SEQUENCE:(\d+)", response.text)
    if not match:
        return None
    names = [line.rsplit("/", 1)[-1] for line in response.text.splitlines()
             if line.endswith(".ts")]
    return int(match.group(1)), names, child


def _remux_pids():
    lines = subprocess.run(
        ["ps", "-eo", "pid=,args="], capture_output=True, text=True, check=True,
    ).stdout.splitlines()
    return [int(line.strip().split(None, 1)[0]) for line in lines
            if "ffmpeg" in line and "hls_time=2" in line and "segment%06d.ts" in line]


def _rss_kib(pid: int):
    status = Path(f"/proc/{pid}/status").read_text()
    return int(re.search(r"^VmRSS:\s+(\d+) kB$", status, re.MULTILINE).group(1))


def _viewer_burst(media_id: str):
    """Each client independently traverses master, child and current segment."""
    def viewer(_index: int):
        http = requests.Session()
        http.verify = False
        observed = []
        bytes_read = 0
        try:
            for _ in range(3):
                start = time.monotonic()
                master = http.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8", timeout=20,
                )
                master.raise_for_status()
                bytes_read += len(master.content)
                child = next(line for line in master.text.splitlines() if line.startswith("/media/"))
                playlist = http.get(config.PLAMENU_URL + child, timeout=20)
                playlist.raise_for_status()
                bytes_read += len(playlist.content)
                manifest = http.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json", timeout=20,
                )
                manifest.raise_for_status()
                assert manifest.json()
                bytes_read += len(manifest.content)
                segment = [line for line in playlist.text.splitlines() if line.endswith(".ts")][-1]
                media = http.get(config.PLAMENU_URL + segment, timeout=20)
                media.raise_for_status()
                assert media.content[0] == 0x47
                bytes_read += len(media.content)
                observed.append(time.monotonic() - start)
        finally:
            http.close()
        return observed, bytes_read

    with ThreadPoolExecutor(max_workers=16) as pool:
        results = list(pool.map(viewer, range(16)))
    timings = [elapsed for samples, _ in results for elapsed in samples]
    timings.sort()
    return {
        "requests": len(timings) * 4,
        "viewers": len(results),
        "bytes": sum(count for _, count in results),
        "p50_ms": round(statistics.median(timings) * 1000),
        "p95_ms": round(timings[int(len(timings) * .95) - 1] * 1000),
        "max_ms": round(max(timings) * 1000),
    }


@pytest.mark.parametrize("dvr_minutes", [5, 30])
def test_native_live_dvr_window_and_viewer_pressure(
    plamenu_api: Api, plamenu_user, db: Db, dvr_minutes: int,
):
    media_dir = Path(tomllib.loads(Path(config.PLAMENU_CONFIG).read_text())["media_dir"])
    if shutil.disk_usage(media_dir).free < 5 * 1024**3:
        pytest.skip("not enough free media storage for a long-window run")
    browser = requests.Session()
    browser.verify = False
    name = unique("longlive")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Long-window live tests"
    )
    status_id = None
    publisher = None
    try:
        with step(f"create a recurring {dvr_minutes}-minute DVR without JavaScript"):
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False, timeout=30,
            )
            assert login.status_code == 303
            form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
            form.raise_for_status()
            csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
            created = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live",
                data={"csrf": csrf, "channel": channel["id"], "title": f"Long DVR {name}",
                      "description": "Accelerated DVR retention and viewer pressure",
                      "language": "en", "visibility": "public", "permanent": "yes",
                      "dvr_minutes": str(dvr_minutes), "latency": "balanced"},
                timeout=30,
            )
            assert created.status_code == 200, created.text[:700]
            status_id = re.search(r'/videos/live/(\d+)', created.text).group(1)
            inputs = re.findall(r'<input type="text" readonly value="([^"]+)"', created.text)
            assert len(inputs) in (3, 4)
            server, key = inputs[0], inputs[2] if len(inputs) == 4 else inputs[1]
            media_id = plamenu_api.get(f"/api/v1/statuses/{status_id}")["media_attachments"][0]["id"]

        with step("push accelerated timestamped media through the real RTMP muxer"):
            publisher = subprocess.Popen(
                ["ffmpeg", "-hide_banner", "-loglevel", "error",
                 "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=10",
                 "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=22050",
                 "-vf", "realtime=speed=20", "-af", "arealtime=speed=20",
                 "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency",
                 "-g", "20", "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "48k",
                 "-f", "flv", f"{server}/{key}"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, stdin=subprocess.PIPE,
            )
            def on_air():
                item = plamenu_api.get(f"/api/v1/statuses/{status_id}")["media_attachments"][0]
                return item if item.get("live", {}).get("state") == "live" else None
            wait_for(
                on_air,
                desc="accelerated publisher goes on air", timeout=60,
            )
            initial = wait_for(
                lambda: (item if (item := _playlist(media_id)) and item[1] else None),
                desc="first committed HLS segment", timeout=60,
            )
            first_name = initial[1][0]
            session_id = db.conn.execute(
                "SELECT session_id FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
                (int(media_id),),
            ).fetchone()[0]
            first_url = (f"{config.PLAMENU_URL}/media/hls/{media_id}/local/"
                         f"{session_id}/{first_name}")
            assert _get(first_url).status_code == 200
            expected = dvr_minutes * 60 // 2
            rolled = wait_for(
                lambda: (item if (item := _playlist(media_id))
                         and item[0] >= 10 and len(item[1]) == expected else None),
                desc=f"{expected}-segment DVR window evicts its first ten segments",
                timeout=240, interval=.5,
            )
            sequence, names, _ = rolled
            log(f"media sequence {sequence}; playlist segments {len(names)}")
            assert _get(first_url).status_code == 404
            assert first_name not in names

        with step("rolling hashes, disk files and one remux remain bounded"):
            digest_response = _get(f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json")
            digest_response.raise_for_status()
            hashes = digest_response.json()
            assert first_name not in hashes
            assert 0 < len(hashes) <= expected
            assert all(re.fullmatch(r"[0-9a-f]{64}", value) for value in hashes.values())
            current_name = next(name for name in reversed(names) if name in hashes)
            current_data = _get(
                f"{config.PLAMENU_URL}/media/hls/{media_id}/local/{session_id}/{current_name}"
            )
            current_data.raise_for_status()
            assert hashes[current_name] == hashlib.sha256(current_data.content).hexdigest()
            scratch = media_dir / "native-live" / str(media_id) / session_id
            segments = list(scratch.glob("segment*.ts"))
            assert len(segments) <= expected + 2, len(segments)
            assert sum(path.stat().st_size for path in segments) < 512 * 1024**2
            remux = _remux_pids()
            assert len(remux) == 1, remux
            before_rss = _rss_kib(remux[0])
            log(f"HLS files {len(segments)}; remux RSS {before_rss} KiB")

        with step("sixteen simultaneous HTTP viewers share the one remux"):
            metrics = _viewer_burst(media_id)
            after = _remux_pids()
            assert after == remux, (remux, after)
            after_rss = _rss_kib(remux[0])
            assert metrics["max_ms"] < 10_000, metrics
            log(f"viewers {metrics['viewers']}, requests {metrics['requests']}, "
                f"served {metrics['bytes']} bytes; median {metrics['p50_ms']} ms, "
                f"p95 {metrics['p95_ms']} ms, max {metrics['max_ms']} ms; "
                f"remux RSS {after_rss} KiB")

        with step("native no-JS rewind can seek the configured historical window"):
            # The accelerated encoder advances media time 20x faster than wall
            # time, so exercise the oldest allowed slice through the media URL.
            slice_number = dvr_minutes // 5 - 1
            bucket = int(time.time() // 60)
            rewind_url = (f"{config.PLAMENU_URL}/media/live/{media_id}/rewind/"
                          f"{bucket}/{slice_number}.mp4")
            def seek_request(_index):
                return requests.get(rewind_url, headers={"Range": "bytes=0-63"},
                                    verify=False, timeout=30)
            with ThreadPoolExecutor(max_workers=16) as pool:
                seeks = list(pool.map(seek_request, range(16)))
            assert all(response.status_code == 206 and len(response.content) == 64
                       for response in seeks)
            clip = _get(rewind_url)
            clip.raise_for_status()
            assert clip.headers["content-type"].startswith("video/mp4")
            assert clip.content[4:8] == b"ftyp"
            probe = subprocess.run(
                ["ffprobe", "-v", "error", "-show_entries", "format=duration",
                 "-of", "csv=p=0", "-i", "pipe:0"],
                input=clip.content, capture_output=True, check=True,
            )
            duration = float(probe.stdout)
            assert 20 < duration <= 301, duration
            assert seeks[0].headers["content-range"].startswith("bytes 0-63/")
            assert len(list((scratch / "rewind").glob(f"{bucket}-{slice_number}.mp4"))) == 1
            assert _get(rewind_url.replace(f"/{slice_number}.mp4", "/99.mp4")).status_code == 404
            log(f"slice {slice_number} duration {duration:.1f}s, "
                f"{len(clip.content)} bytes; 16 concurrent byte-range seeks 206")

        with step("stop and publish the long recording without leaving live scratch"):
            _, error = publisher.communicate(input=b"q\n", timeout=30)
            assert publisher.returncode == 0, error.decode(errors="replace")[-700:]
            publisher = None
            wait_for(
                lambda: db.conn.execute(
                    "SELECT replay_media_id FROM native_live_sessions WHERE media_id = %s AND session_id = %s",
                    (int(media_id), session_id),
                ).fetchone()[0],
                desc="long-window replay is saved", timeout=180,
            )
            assert _get(f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8").status_code == 404
    finally:
        if publisher is not None:
            publisher.terminate()
            try:
                publisher.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                publisher.kill()
                publisher.communicate(timeout=10)
        if status_id is not None:
            try:
                plamenu_api.delete(f"/api/v1/statuses/{status_id}")
            except Exception:
                pass
