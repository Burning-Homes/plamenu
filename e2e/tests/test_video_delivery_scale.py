"""Opt-in sustained mixed delivery budget; never touches the standing server."""

import json
import os
import re
import socket
import statistics
import subprocess
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from test_video_backup_restore import _csrf, _digest, _stop_process

DURATION = 300
LIVE_VIEWERS = 32
RANGE_VIEWERS = 16
RANGE_BYTES = 65536
MIB = 1024 * 1024


def _children(pid):
    result = set()
    try:
        tasks = list(Path(f"/proc/{pid}/task").iterdir())
    except FileNotFoundError:
        return result
    for task in tasks:
        try:
            result.update(
                int(child) for child in (task / "children").read_text().split()
            )
        except FileNotFoundError:
            pass
    return result


def _process(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        rss = int(Path(f"/proc/{pid}/statm").read_text().split()[1]) * os.sysconf(
            "SC_PAGE_SIZE"
        )
        return {
            "identity": (pid, fields[19]),
            "rss": rss,
            "cpu": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
        }
    except FileNotFoundError:
        return None


def _tree_bytes(root):
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file():
                total += path.stat().st_size
        except FileNotFoundError:
            pass  # rolling HLS eviction may race measurement
    return total


@pytest.mark.capability("AP09_SCALE_VERIFY")
@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="sustained native ranges use the file offered by actual PeerTube",
)
def test_sustained_native_live_and_exact_peer_ranges_have_bounded_resources():
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable instance port is occupied"
        )
    label = unique("deliveryscale").replace("-", "_")
    peer = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-delivery-scale"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    start, finish = threading.Event(), threading.Event()
    condition = threading.Condition()
    joined = 0
    publisher = pool = channel = api = admin = None
    subscribed = suspended = False
    samples = []
    try:
        with step("prepare real native film, PeerTube offer and opted-in live creator"):
            peer.start(suite_settings=True)
            _stop_process(peer)
            with socket.socket() as allocator:
                allocator.bind(("127.0.0.1", 0))
                port = allocator.getsockname()[1]
            peer._config_path.write_text(
                peer._config_path.read_text().replace(
                    'live_ingest_bind = "127.0.0.1:0"',
                    f'live_ingest_bind = "127.0.0.1:{port}"',
                )
            )
            peer._launch(120, append_log=True)
            user, api = peer.user_api("scalecaster")
            _, admin = peer.admin("scaleoperator")
            owner = api.get("/api/v1/accounts/verify_credentials")["id"]
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("scalefilms")[:30],
                display_name="Scale films",
            )
            handle = f"{channel['username']}@{peer.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: peer.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube follows scale creator",
            )
            assert (
                browser.post(
                    f"{peer.url}/login",
                    data={"email": user.email, "password": user.password},
                    allow_redirects=False,
                    timeout=30,
                ).status_code
                == 303
            )
            csrf = _csrf(browser.get(f"{peer.url}/videos", timeout=30).text)
            movie = peertube.make_sample_mp4(
                evidence / "sample.mp4", seconds=8, size="640x480"
            )
            title = f"Sustained ranges {label}"
            fields = {
                "csrf": csrf,
                "channel": channel["id"],
                "title": title,
                "description": "Moving pattern and tone",
                "visibility": "public",
                "language": "en",
                "alt": "Moving pattern",
                "op": "review",
            }
            with movie.open("rb") as stream:
                uploaded = browser.post(
                    f"{peer.url}/web/videos",
                    files=[
                        *((key, (None, str(value))) for key, value in fields.items()),
                        ("file", ("sample.mp4", stream, "video/mp4")),
                    ],
                    allow_redirects=False,
                    timeout=90,
                )
            assert uploaded.status_code == 303
            media = urlsplit(
                urljoin(peer.url, uploaded.headers["Location"])
            ).path.rsplit("/", 1)[1]
            wait_for(
                lambda: peer.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                    int(media),
                ),
                desc="actual film processing completes",
                timeout=180,
            )
            api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=media,
                title=title,
                visibility="public",
            )
            remote = wait_for(
                lambda: next(
                    (
                        video
                        for video in pt.get(
                            "/api/v1/users/me/subscriptions/videos",
                            count=100,
                            sort="-publishedAt",
                        )["data"]
                        if video["name"] == title
                    ),
                    None,
                ),
                desc="actual PeerTube imports scale film",
            )
            file_url = pt.video(remote["uuid"])["files"][0]["fileUrl"]
            body = requests.get(file_url, verify=False, timeout=30)
            assert body.status_code == 200 and len(body.content) > RANGE_BYTES
            finite = body.content
            endpoint = f"/api/v1/video_channels/{channel['id']}/analytics"
            api._request("PUT", endpoint, json={"enabled": True, "retention_days": 1})
            created = browser.post(
                f"{peer.url}/web/videos/live",
                data={
                    "csrf": csrf,
                    "channel": channel["id"],
                    "title": f"Sustained live {label}",
                    "description": "Moving pattern and tone",
                    "visibility": "public",
                    "permanent": "yes",
                    "dvr_minutes": "5",
                    "latency": "balanced",
                },
                timeout=30,
            )
            assert created.status_code == 200
            status = re.search(r"/videos/live/(\d+)", created.text).group(1)
            server, key, _https = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            live = api.get_status(status)["media_attachments"][0]["id"]
            root = Path(peer._dir.name) / "media"
            initial_size = _tree_bytes(root)
            baseline = _process(peer._proc.pid)["rss"]
            publisher = subprocess.Popen(
                [
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x360:rate=25",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "2",
                    "-preset",
                    "veryfast",
                    "-tune",
                    "zerolatency",
                    "-g",
                    "50",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-t",
                    "480",
                    "-f",
                    "flv",
                    f"{server}/{key}",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            wait_for(
                lambda: peer.db_value(
                    "SELECT live_state='live' FROM media_attachments WHERE id=%s",
                    int(live),
                ),
                desc="sustained publisher is on air",
            )

        def live_viewer():
            nonlocal joined
            requested = time.monotonic()
            result = {
                "bytes": 0,
                "first_byte_seconds": None,
                "max_gap_seconds": 0,
                "ended_at": None,
            }
            with requests.get(
                f"{peer.url}/media/live/{live}/stream.mp4",
                stream=True,
                verify=False,
                timeout=(10, 20),
            ) as response:
                assert (
                    response.status_code == 200
                    and response.headers["Cache-Control"] == "no-store"
                )
                previous = None
                for chunk in response.iter_content(4096):
                    now = time.monotonic()
                    if previous is None:
                        assert b"ftyp" in chunk[:32]
                        result["first_byte_seconds"] = now - requested
                        assert result["first_byte_seconds"] <= 20
                        with condition:
                            joined += 1
                            condition.notify_all()
                    else:
                        result["max_gap_seconds"] = max(
                            result["max_gap_seconds"], now - previous
                        )
                    previous = now
                    result["bytes"] += len(chunk)
                result["ended_at"] = time.monotonic()
            return result

        def range_viewer(index):
            start.wait(timeout=30)
            count, delivered, times = 0, 0, []
            with requests.Session() as session:
                session.verify = False
                while not finish.is_set():
                    offset = (count * 7919 + index * 104729) % (
                        len(finite) - RANGE_BYTES + 1
                    )
                    before = time.monotonic()
                    response = session.get(
                        file_url,
                        headers={"Range": f"bytes={offset}-{offset + RANGE_BYTES - 1}"},
                        timeout=10,
                    )
                    elapsed = time.monotonic() - before
                    if finish.is_set():
                        break  # suspension deliberately revokes the next request
                    assert response.status_code == 206
                    assert response.content == finite[offset : offset + RANGE_BYTES]
                    times.append(elapsed)
                    count += 1
                    delivered += len(response.content)
                    finish.wait(max(0, 1 - elapsed))
            return {"requests": count, "bytes": delivered, "latencies": times}

        with step(
            "sustain 32 live responses and 16 exact-range clients for five minutes"
        ):
            pool = ThreadPoolExecutor(max_workers=LIVE_VIEWERS + RANGE_VIEWERS)
            consumers = [pool.submit(live_viewer) for _ in range(LIVE_VIEWERS)]
            ranges = [
                pool.submit(range_viewer, index) for index in range(RANGE_VIEWERS)
            ]
            with condition:
                assert condition.wait_for(
                    lambda: (
                        joined == LIVE_VIEWERS or any(job.done() for job in consumers)
                    ),
                    timeout=25,
                )
                assert joined == LIVE_VIEWERS, (
                    "all concurrent first viewers must join without retries"
                )
            started = time.monotonic()
            start.set()
            previous_cpu = {}
            cpu_seconds = 0
            publisher_cpu_start = _process(publisher.pid)["cpu"]
            while time.monotonic() - started < DURATION:
                processes = [
                    _process(pid)
                    for pid in {peer._proc.pid, *_children(peer._proc.pid)}
                ]
                processes = [process for process in processes if process is not None]
                for process in processes:
                    identity = process["identity"]
                    if identity in previous_cpu:
                        cpu_seconds += max(0, process["cpu"] - previous_cpu[identity])
                    previous_cpu[identity] = process["cpu"]
                remuxes = 0
                for pid in _children(peer._proc.pid):
                    try:
                        remuxes += (
                            f"/media/hls/{live}/master.m3u8".encode()
                            in Path(f"/proc/{pid}/cmdline").read_bytes()
                        )
                    except FileNotFoundError:
                        pass
                assert remuxes == 1, "viewers must share one unchanged remux"
                assert not any(job.done() for job in consumers), (
                    "attached live response ended before intentional suspension"
                )
                for job in ranges:
                    if job.done():
                        job.result()  # propagate exact-byte/response failure immediately
                        pytest.fail("range client ended early")
                samples.append(
                    {
                        "seconds": time.monotonic() - started,
                        "server_rss": _process(peer._proc.pid)["rss"],
                        "service_rss": sum(p["rss"] for p in processes),
                        "owned_media_bytes": _tree_bytes(root),
                        "remux_children": remuxes,
                        "publisher_rss": _process(publisher.pid)["rss"],
                    }
                )
                time.sleep(1)
            elapsed = time.monotonic() - started
            publisher_cpu = (
                _process(publisher.pid)["cpu"] - publisher_cpu_start
            ) / elapsed
            assert elapsed >= DURATION
            totals = api.get(endpoint)["daily"]
            assert {row["lane"] for row in totals} >= {"file", "live"}

        with step(
            "actual owner moderation terminates every attached consumer and refuses new ranges"
        ):
            finish.set()
            revoked = time.monotonic()
            admin.post(
                f"/api/v1/admin/accounts/{owner}/action",
                type="suspend",
                text="Disposable sustained-delivery check",
            )
            suspended = True
            streams = [job.result(timeout=10) for job in consumers]
            seeks = [job.result(timeout=15) for job in ranges]
            assert all(0 <= result["ended_at"] - revoked <= 10 for result in streams)
            assert (
                requests.get(
                    file_url, headers={"Range": "bytes=0-63"}, verify=False, timeout=10
                ).status_code
                == 404
            )
            assert (
                requests.get(
                    f"{peer.url}/media/live/{live}/stream.mp4", verify=False, timeout=10
                ).status_code
                == 404
            )
            times = sorted(value for result in seeks for value in result["latencies"])
            p95 = times[int((len(times) - 1) * 0.95)]
            result = {
                "candidate_sha256": _digest(ephemeral.binary_path()),
                "duration_seconds": elapsed,
                "live_viewers": LIVE_VIEWERS,
                "range_viewers": RANGE_VIEWERS,
                "range_requests": sum(value["requests"] for value in seeks),
                "range_p95_seconds": p95,
                "range_body_bytes": sum(value["bytes"] for value in seeks),
                "live_body_bytes": sum(value["bytes"] for value in streams),
                "max_live_start_seconds": max(
                    value["first_byte_seconds"] for value in streams
                ),
                "max_live_chunk_gap_seconds": max(
                    value["max_gap_seconds"] for value in streams
                ),
                "max_service_rss": max(sample["service_rss"] for sample in samples),
                "server_rss_growth": max(sample["server_rss"] for sample in samples)
                - baseline,
                "late_server_rss_growth": statistics.mean(
                    sample["server_rss"] for sample in samples[-75:]
                )
                - statistics.mean(sample["server_rss"] for sample in samples[:75]),
                "mean_service_cpu_cores": cpu_seconds / elapsed,
                "mean_test_publisher_cpu_cores": publisher_cpu,
                "max_test_publisher_rss": max(
                    sample["publisher_rss"] for sample in samples
                ),
                "max_media_growth": max(
                    sample["owned_media_bytes"] for sample in samples
                )
                - initial_size,
                "max_revocation_eof_seconds": max(
                    value["ended_at"] - revoked for value in streams
                ),
                "measurements": "origin body/range arrivals, not decoded playback or P2P savings",
                "host_cpu_count": os.cpu_count(),
                "samples": samples,
            }
            record = evidence / "result.json"
            record.write_text(json.dumps(result, indent=2))
            record.chmod(0o600)
            assert result["range_requests"] >= 4000 and p95 <= 1
            assert result["max_live_chunk_gap_seconds"] <= 15
            assert result["max_service_rss"] <= 512 * MIB
            assert result["server_rss_growth"] <= 256 * MIB
            assert result["late_server_rss_growth"] <= 32 * MIB
            assert result["mean_service_cpu_cores"] <= 2
            assert result["max_media_growth"] <= 2 * 1024 * MIB
            print(f"Protected sustained-delivery evidence: {evidence}")
    finally:
        start.set()
        finish.set()
        # End owned publishers before joining any still-blocked consumers.
        if publisher is not None:
            publisher.terminate()
            try:
                publisher.wait(timeout=15)
            except subprocess.TimeoutExpired:
                publisher.kill()
                publisher.wait(timeout=10)
        try:
            if suspended:
                admin.post(f"/api/v1/admin/accounts/{owner}/unsuspend")
            if (
                channel is not None
                and peer._proc is not None
                and peer._proc.poll() is None
            ):
                api.post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            if peer._dir is not None:
                log = Path(peer._dir.name) / "server.log"
                if log.exists():
                    retained = evidence / "server.log"
                    retained.write_text(log.read_text())
                    retained.chmod(0o600)
            peer.stop()
            if pool is not None:
                pool.shutdown(wait=True, cancel_futures=True)
            browser.close()
