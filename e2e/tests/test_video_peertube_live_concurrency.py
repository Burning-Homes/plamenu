"""Two actual PeerTube-origin P2P-capable broadcasts / eight real decoded viewers.

Only the owned browser tracker URLs map through the owned TCP bridge. The
actual PeerTube tracker/upstream signaling stays in use and is not stopped or
globally configured. HTTP media/codec/checksums remain real and unchanged.
"""

import json
import os
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
from tests.test_video_backup_restore import _stop_process
from tests.test_video_delivery_scale import _children, _process, _tree_bytes
from tests.test_video_live_backup_restore import _stop_encoder
from tests.test_video_live_concurrency import _clock_publisher, _port
from tests.test_video_offload import _freeze


@pytest.mark.skipif(
    os.environ.get("AP09_CONCURRENCY_VERIFY") != "1",
    reason="owned actual PeerTube concurrency/TCP fault drill",
)
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    reverse_of="test_two_broadcasts_eight_decoders_survive_real_tracker_tcp_loss",
)
def test_two_peertube_broadcasts_eight_decoders_survive_scoped_tracker_tcp_loss(
    monkeypatch,
):
    assert peertube.reachable()
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("ptconcurrency").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-peertube-concurrency"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=parent))
    evidence.chmod(0o700)
    source = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    token = secrets.token_hex(32)
    environment = {
        **os.environ,
        "AP09_PROXY_TOKEN": token,
        "PEERTUBE_E2E_USER": peertube.ROOT_USER,
        "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
    }
    proxy = observers = None
    logs, videos, publishers, samples = [], [], [], []
    origin_samples = []
    pt = peertube.root()
    control = f"http://127.0.0.1:{_port()}"
    try:
        with step(
            "one frozen native consumer and scoped bridge to actual PeerTube's private tracker backend"
        ):
            source.start()
            candidate = _freeze(source, evidence, monkeypatch)
            _stop_process(source)
            upstream = _port()
            source._config_path.write_text(
                source._config_path.read_text().replace(
                    'bind = "0.0.0.0:8422"', f'bind = "127.0.0.1:{upstream}"'
                )
            )
            source._config_path.chmod(0o600)
            # Resolve the service owned by the configured fixture. Isolated
            # release fleets use distinct Compose projects and fresh volumes.
            service = subprocess.run(
                [
                    "docker",
                    "compose",
                    "--file",
                    str(config.PEERTUBE_DIR / "compose.dev.yml"),
                    "ps",
                    "--quiet",
                    "peertube",
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=15,
            )
            containers = service.stdout.split()
            assert len(containers) == 1, "exactly one configured PeerTube service"
            inspected = subprocess.run(
                [
                    "docker",
                    "inspect",
                    containers[0],
                    "--format",
                    "{{json .NetworkSettings.Networks}}",
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=15,
            )
            peertube_networks = json.loads(inspected.stdout)
            caddy = subprocess.run(
                [
                    "docker",
                    "inspect",
                    "mastodon-test-caddy-1",
                    "--format",
                    "{{json .NetworkSettings.Networks}}",
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=15,
            )
            shared = set(peertube_networks) & set(json.loads(caddy.stdout))
            addresses = {
                peertube_networks[name]["IPAddress"]
                for name in shared
                if peertube_networks[name]["IPAddress"]
            }
            assert len(addresses) == 1, (
                "choose the exact owned fixture network rather than guessing"
            )
            tracker_host = addresses.pop()
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
                    control.rsplit(":", 1)[1],
                    "--tracker-host",
                    tracker_host,
                    "--tracker-port",
                    "9000",
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
                        control + "/status",
                        headers={"X-AP09-Control": token},
                        timeout=1,
                    ).ok
                except requests.RequestException:
                    return False

            wait_for(
                bridge_ready,
                desc="owned native-media/actual-PeerTube-tracker TCP bridge listens",
                timeout=10,
            )
            source._launch(120, append_log=True)
            source.apply_suite_settings()
            _, api = source.user_api("ptconcurrentviewer")
            root = api.resolve_account(f"root@{config.PEERTUBE_DOMAIN}")
            api.follow(root["id"])

        with step(
            "two actual PeerTube normal-latency P2P broadcasts carry decoded pixel clocks"
        ):
            manifest, media_ids = [], []
            for index in range(2):
                # PeerTube itself disables peer delivery in small-latency mode
                # 3. Mode 2 is its compatible normal-latency peer surface.
                video = pt.create_live(
                    f"PeerTube concurrency clock {label} {index}", latency_mode=2
                )
                videos.append(video)
                settings = pt.live_config(video["id"])
                publishers.append(
                    _clock_publisher(
                        settings["rtmpUrl"].rstrip("/"),
                        settings["streamKey"],
                        size="640x360",
                    )
                )
                pt.wait_for_state(video["uuid"], 1, timeout=90)

                def on_air(video=video):
                    status = api.resolve_status(pt.ap_id(video["uuid"]))
                    return (
                        status
                        if status["media_attachments"][0].get("live", {}).get("state")
                        == "live"
                        else None
                    )

                status = wait_for(
                    on_air,
                    desc="owned consumer sees actual PeerTube on-air Video",
                    timeout=90,
                )
                live_files = [
                    file
                    for playlist in pt.video(video["uuid"])["streamingPlaylists"]
                    for file in playlist["files"]
                ]
                heights = sorted(
                    {
                        file["resolution"]["id"]
                        for file in live_files
                        if file["resolution"]["id"] > 0
                    }
                )
                media_ids.append(int(status["media_attachments"][0]["id"]))
                manifest.append(
                    {
                        "local": f"{source.url}/web/statuses/{status['id']}",
                        "remote": f"{config.PEERTUBE_URL}/w/{video['uuid']}",
                        # Request an actually advertised rung, not an invented
                        # VOD ladder. Unlabelled original is our 640x360 publisher;
                        # the browser must still prove its actual decoded height.
                        "qualityHeight": heights[0] if heights else 360,
                        "qualitySelectable": len(heights) > 1,
                        "nativeWarmDelayMs": 5000,
                        "warmPeerDvr": True,
                        "advertisedLiveHeights": heights,
                        "trackerOrigin": f"wss://{config.PEERTUBE_DOMAIN}/tracker/socket",
                        "trackerVia": f"wss://{source.domain}/tracker/socket",
                    }
                )
            path = evidence / "viewer-manifest.json"
            path.write_text(json.dumps(manifest, indent=2))

        with step(
            "eight simultaneous decoders sustain real scoped tracker loss and restoration"
        ):
            browser_log = (evidence / "browser.log").open("w")
            logs.append(browser_log)
            observers = subprocess.Popen(
                [
                    "node",
                    str(
                        config.PLAMENU_DIR / "accessibility/verify-live-concurrency.mjs"
                    ),
                    str(path),
                    control,
                    str(evidence),
                ],
                env=environment,
                stdout=browser_log,
                stderr=subprocess.STDOUT,
            )
            started = time.monotonic()
            origin_observer = (
                (
                    config.PLAMENU_DIR / "accessibility/origin-file-observer.mjs"
                ).read_text()
                if os.environ.get("AP09_CONCURRENCY_TRACE") == "1"
                else None
            )
            previous_cpu, cpu_seconds = {}, 0
            while observers.poll() is None:
                assert time.monotonic() - started <= 420, (
                    "finite concurrent browser deadline"
                )
                assert source._proc.poll() is None and proxy.poll() is None
                assert all(publisher.poll() is None for publisher in publishers)
                children = _children(source._proc.pid)
                processes = [
                    p
                    for pid in {source._proc.pid, *children}
                    if (p := _process(pid)) is not None
                ]
                for process in processes:
                    identity = process["identity"]
                    if identity in previous_cpu:
                        cpu_seconds += max(0, process["cpu"] - previous_cpu[identity])
                    previous_cpu[identity] = process["cpu"]
                remuxers = 0
                for pid in children:
                    try:
                        remuxers += (
                            Path(f"/proc/{pid}/comm").read_text().strip() == "ffmpeg"
                        )
                    except FileNotFoundError:
                        pass
                samples.append(
                    {
                        "seconds": time.monotonic() - started,
                        "service_rss": sum(p["rss"] for p in processes),
                        "ffmpeg_children": remuxers,
                        "owned_media_bytes": _tree_bytes(
                            Path(source._dir.name) / "media"
                        ),
                    }
                )
                if (
                    origin_observer
                    and len(samples) % 2 == 0
                    and len(origin_samples) < 40
                ):
                    try:
                        observed = subprocess.run(
                            [
                                "docker",
                                "exec",
                                "-i",
                                containers[0],
                                "node",
                                "--input-type=module",
                                "-",
                                "--owned-origin-observation",
                                *(video["uuid"] for video in videos),
                            ],
                            input=origin_observer,
                            capture_output=True,
                            text=True,
                            timeout=2,
                            check=False,
                        )
                        if observed.returncode == 0:
                            origin_samples.append(json.loads(observed.stdout))
                        else:
                            origin_samples.append({"unavailable": True})
                    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
                        origin_samples.append({"unavailable": True})
                assert remuxers <= 2, (
                    "remote viewers must not create a per-viewer encode/remux"
                )
                time.sleep(1)
            elapsed = time.monotonic() - started
            summary = {
                "candidate_sha256": candidate,
                "duration_seconds": elapsed,
                "origin": "actual PeerTube 8.3, P2P-capable normal-latency mode 2",
                "max_service_rss": max(s["service_rss"] for s in samples),
                "mean_service_cpu_cores": cpu_seconds / elapsed,
                "max_ffmpeg_children": max(s["ffmpeg_children"] for s in samples),
                "samples": samples,
                "measurement_scope": "native consumer and children; PeerTube, test publishers, browser and bridge excluded",
                "budgets": {
                    "mean_service_cpu_cores": 3,
                    "max_service_rss": 768 * 1024**2,
                    "max_ffmpeg_children": 2,
                    "browser_deadline_seconds": 420,
                    "decoded_native_p95_glass_ms": 10000,
                },
            }
            (evidence / "resource-result.json").write_text(
                json.dumps(summary, indent=2)
            )
            assert observers.returncode == 0, (
                "inspect protected eight-decoder/TCP observations"
            )
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
            )
            print(f"Protected actual PeerTube-origin concurrency evidence: {evidence}")
    finally:
        if origin_samples:
            path = evidence / "origin-publication.json"
            path.write_text(json.dumps(origin_samples, indent=2))
            path.chmod(0o600)
        if samples:
            (evidence / "resource-progress.json").write_text(
                json.dumps(samples, indent=2)
            )
        if observers is not None and observers.poll() is None:
            _stop_encoder(observers)
        for publisher in publishers:
            _stop_encoder(publisher)
        for video in videos:
            pt.delete_video(video["id"])
        try:
            if source._dir is not None:
                log = Path(source._dir.name) / "server.log"
                if log.exists():
                    (evidence / "origin.log").write_text(log.read_text())
            source.stop()
        finally:
            _stop_encoder(proxy)
            for log in logs:
                log.close()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
