"""Two owned RTMP broadcasts, eight real decoders, and a tracker TCP fault.

The disposable bridge interrupts only this fixture's tracker sockets; actual
HTTP media, codecs, PeerTube and WebRTC remain in use. Resource figures exclude
test publishers/browser/bridge, whose PIDs are recorded separately. This is a
small local workload, not a production capacity or WAN/hardware certification.
"""

import json
import os
import re
import secrets
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _digest, _stop_process
from tests.test_video_delivery_scale import _children, _process, _tree_bytes
from tests.test_video_live_backup_restore import _stop_encoder


def _port():
    with socket.socket() as allocator:
        allocator.bind(("127.0.0.1", 0))
        return allocator.getsockname()[1]


def _clock_publisher(server, key, *, size="320x180"):
    # Thirty-two ten-pixel luminance cells encode wall-clock deciseconds in
    # decoded pixels. Keys stay private, never in logs, evidence or assertions.
    assert size in ("320x180", "640x360"), "finite predeclared pixel-clock sources"
    multiplier = 2 if size == "640x360" else 1
    pattern = (
        f"testsrc2=size={size}:rate=15,geq="
        rf"lum=if(lt(Y\,{12 * multiplier})\,if(eq(bitand(floor(time(0)*10)\,pow(2\,floor(X/{10 * multiplier})))\,0)\,16\,235)\,lum(X\,Y)):"
        rf"cb=if(lt(Y\,{12 * multiplier})\,128\,cb(X\,Y)):cr=if(lt(Y\,{12 * multiplier})\,128\,cr(X\,Y))"
    )
    return subprocess.Popen(
        [
            "ffmpeg",
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            pattern,
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
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-t",
            "600",
            "-f",
            "flv",
            f"{server}/{key}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


@pytest.mark.capability("AP09_CONCURRENCY_VERIFY")
@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    reverse_of="test_two_peertube_broadcasts_eight_decoders_survive_scoped_tracker_tcp_loss",
)
def test_two_broadcasts_eight_decoders_survive_real_tracker_tcp_loss(monkeypatch):
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable fixture port is occupied"
        )
    label = unique("liveconcurrency").replace("-", "_")
    source = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-live-concurrency"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    token = secrets.token_hex(32)
    environment = {
        **os.environ,
        "AP09_PROXY_TOKEN": token,
        "PEERTUBE_E2E_USER": peertube.ROOT_USER,
        "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
    }
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    subscribed = False
    channel = api = proxy = observers = None
    publishers, logs, samples = [], [], []
    control_port = _port()
    control_url = f"http://127.0.0.1:{control_port}"
    try:
        with step("start only owned native server and real tracker fault bridge"):
            source.start(suite_settings=True)
            from tests.test_video_offload import _freeze

            _freeze(source, evidence, monkeypatch)
            _stop_process(source)
            upstream, rtmp_port = _port(), _port()
            source._config_path.write_text(
                source._config_path.read_text()
                .replace('bind = "0.0.0.0:8422"', f'bind = "127.0.0.1:{upstream}"')
                .replace(
                    'live_ingest_bind = "127.0.0.1:0"',
                    f'live_ingest_bind = "127.0.0.1:{rtmp_port}"',
                )
            )
            source._config_path.chmod(0o600)
            proxy_log = (evidence / "proxy.log").open("w")
            logs.append(proxy_log)
            proxy = subprocess.Popen(
                [
                    str(config.PLAMENU_DIR / "e2e/.venv/bin/python"),
                    str(config.PLAMENU_DIR / "e2e/tools/tracker_fault_proxy.py"),
                    "--listen",
                    "8422",
                    "--upstream",
                    str(upstream),
                    "--control",
                    str(control_port),
                    "--result",
                    str(evidence / "proxy.json"),
                ],
                env=environment,
                stdout=proxy_log,
                stderr=subprocess.STDOUT,
            )

            def bridge_ready():
                try:
                    return requests.post(
                        f"{control_url}/status",
                        headers={"X-AP09-Control": token},
                        timeout=1,
                    ).ok
                except requests.RequestException:
                    return False

            wait_for(bridge_ready, desc="owned TCP bridge is listening", timeout=10)
            source._launch(120, append_log=True)
            user, api = source.user_api("concurrentcaster")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("concurrentlive")[:30],
                display_name="Concurrent native broadcasts",
            )
            handle = f"{channel['username']}@{source.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: source.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube subscribes before broadcasts",
            )
            assert (
                browser.post(
                    f"{source.url}/login",
                    data={"email": user.email, "password": user.password},
                    allow_redirects=False,
                    timeout=30,
                ).status_code
                == 303
            )

        with step("publish two independent moving pixel-clock broadcasts"):
            manifest, media_ids = [], []
            for index in range(2):
                title = f"Concurrent clock {label} {index}"
                created = browser.post(
                    f"{source.url}/web/videos/live",
                    data={
                        "csrf": _csrf(
                            browser.get(f"{source.url}/videos", timeout=30).text
                        ),
                        "channel": channel["id"],
                        "title": title,
                        "description": "Moving pattern, pixel clock and steady tone",
                        "visibility": "public",
                        "permanent": "yes",
                        "latency": "balanced",
                        "dvr_minutes": "5",
                    },
                    timeout=30,
                )
                assert created.status_code == 200
                status = re.search(r"/videos/live/(\d+)", created.text).group(1)
                server, key, _https = re.findall(
                    r'<input type="text" readonly value="([^"]+)"', created.text
                )
                media = int(api.get_status(status)["media_attachments"][0]["id"])
                media_ids.append(media)
                publishers.append(_clock_publisher(server, key))
                wait_for(
                    lambda media=media: source.db_value(
                        "SELECT live_state='live' FROM media_attachments WHERE id=%s",
                        media,
                    ),
                    desc="native clock broadcast is on air",
                    timeout=45,
                )
                remote = wait_for(
                    lambda title=title: next(
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
                    desc="actual PeerTube imports the live broadcast",
                    timeout=90,
                )
                manifest.append(
                    {
                        "local": f"{source.url}/web/statuses/{status}",
                        "remote": f"{config.PEERTUBE_URL}/w/{remote['uuid']}",
                    }
                )
            manifest_path = evidence / "viewer-manifest.json"
            manifest_path.write_text(json.dumps(manifest, indent=2))
            manifest_path.chmod(0o600)

        with step(
            "eight decoders sustain two broadcasts and real tracker outage within declared budgets"
        ):
            viewer_log = (evidence / "browser.log").open("w")
            logs.append(viewer_log)
            observers = subprocess.Popen(
                [
                    "node",
                    "verify-live-concurrency.mjs",
                    str(manifest_path),
                    control_url,
                    str(evidence),
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                env=environment,
                stdout=viewer_log,
                stderr=subprocess.STDOUT,
            )
            started = time.monotonic()
            previous_cpu, cpu_seconds = {}, 0
            while observers.poll() is None:
                assert time.monotonic() - started <= 420, (
                    "concurrent browser drill exceeded finite deadline"
                )
                assert source._proc.poll() is None and proxy.poll() is None
                assert all(publisher.poll() is None for publisher in publishers), (
                    "owned input publisher ended prematurely"
                )
                children = _children(source._proc.pid)
                processes = [_process(pid) for pid in {source._proc.pid, *children}]
                processes = [process for process in processes if process is not None]
                for process in processes:
                    identity = process["identity"]
                    if identity in previous_cpu:
                        cpu_seconds += max(0, process["cpu"] - previous_cpu[identity])
                    previous_cpu[identity] = process["cpu"]
                encoders = 0
                for pid in children:
                    try:
                        encoders += (
                            Path(f"/proc/{pid}/comm").read_text().strip() == "ffmpeg"
                        )
                    except FileNotFoundError:
                        pass
                samples.append(
                    {
                        "seconds": time.monotonic() - started,
                        "service_rss": sum(process["rss"] for process in processes),
                        "ffmpeg_children": encoders,
                        "owned_media_bytes": _tree_bytes(
                            Path(source._dir.name) / "media"
                        ),
                        "test_publishers": [
                            _process(publisher.pid) for publisher in publishers
                        ],
                        "test_browser_driver": _process(observers.pid),
                        "test_bridge": _process(proxy.pid),
                    }
                )
                assert encoders <= 4, (
                    "a new encoder/remux must not be created per viewer"
                )
                time.sleep(1)
            elapsed = time.monotonic() - started
            summary = {
                "candidate_sha256": _digest(ephemeral.binary_path()),
                "duration_seconds": elapsed,
                "mean_service_cpu_cores": cpu_seconds / elapsed,
                "max_service_rss": max(sample["service_rss"] for sample in samples),
                "max_ffmpeg_children": max(
                    sample["ffmpeg_children"] for sample in samples
                ),
                "host_cpu_count": os.cpu_count(),
                "samples": samples,
                "measurement_scope": "server and direct children only; publishers/browser/bridge excluded",
                "budgets": {
                    "mean_service_cpu_cores": 3,
                    "max_service_rss": 768 * 1024**2,
                    "max_ffmpeg_children": 4,
                    "browser_deadline_seconds": 420,
                },
            }
            result = evidence / "resource-result.json"
            result.write_text(json.dumps(summary, indent=2))
            result.chmod(0o600)
            assert observers.returncode == 0, (evidence / "browser.log").read_text()
            assert summary["max_service_rss"] <= summary["budgets"]["max_service_rss"]
            assert (
                summary["mean_service_cpu_cores"]
                <= summary["budgets"]["mean_service_cpu_cores"]
            )
            assert all(
                source.db_value(
                    "SELECT live_state='live' FROM media_attachments WHERE id=%s", media
                )
                for media in media_ids
            ), "tracker loss must not end either publisher's session"
            print(f"Protected concurrent decoder and TCP-fault evidence: {evidence}")
    finally:
        if samples:
            (evidence / "resource-progress.json").write_text(
                json.dumps(samples, indent=2)
            )
        try:
            requests.post(
                f"{control_url}/restore", headers={"X-AP09-Control": token}, timeout=2
            )
        except requests.RequestException:
            pass
        if observers is not None and observers.poll() is None:
            _stop_encoder(observers)
        for publisher in publishers:
            _stop_encoder(publisher)
        try:
            if (
                channel is not None
                and source._proc is not None
                and source._proc.poll() is None
            ):
                api.post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            if source._dir is not None:
                log = Path(source._dir.name) / "server.log"
                if log.exists():
                    retained = evidence / "server.log"
                    retained.write_text(log.read_text())
                    retained.chmod(0o600)
            source.stop()
            if proxy is not None:
                _stop_encoder(proxy)
            for log in logs:
                log.close()
            for path in evidence.iterdir():
                if path.is_file():
                    path.chmod(0o600)
