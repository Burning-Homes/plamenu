"""Matched eight-viewer HTTP-body savings; both actual source products.

The counted origin is the native delivery service. For a PeerTube publication
it is the Plamenu consumer: no claim about PeerTube's unobserved upstream bytes.
"""
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _stop_process
from tests.test_video_delivery_scale import _children, _process
from tests.test_video_live_concurrency import _port
from tests.test_video_live_backup_restore import _stop_encoder
from tests.test_video_offload import _freeze, _submit


@pytest.mark.skipif(os.environ.get("AP09_PEER_BYTES_VERIFY") != "1", reason="owned matched eight-viewer real HTTP-body/RTC measurement")
@pytest.mark.federation(direction="both", peer="peertube")
@pytest.mark.parametrize("product", ["native", "peertube"])
def test_both_source_products_reduce_native_http_emission_including_seed_cost(product, monkeypatch):
    assert peertube.reachable()
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("peerbytes").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-peer-bytes"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-{product}-", dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(domain=config.PLAMENU2_DOMAIN, bind="0.0.0.0:8422", db_name=f"plamenu_ap09_{label}")
    token = secrets.token_hex(32)
    pt = peertube.root()
    remote = proxy = observer = None
    logs, samples = [], []
    subscribed = False
    control = f"http://127.0.0.1:{_port()}"
    environment = {**os.environ, "AP09_PROXY_TOKEN": token}
    try:
        with step("owned frozen native service behind an actual body-counting constrained TCP bridge"):
            origin.start()
            candidate = _freeze(origin, evidence, monkeypatch)
            _stop_process(origin)
            upstream = _port()
            origin._config_path.write_text(origin._config_path.read_text().replace(
                'bind = "0.0.0.0:8422"', f'bind = "127.0.0.1:{upstream}"'))
            origin._config_path.chmod(0o600)
            proxy_log = (evidence / "proxy.log").open("w")
            logs.append(proxy_log)
            proxy = subprocess.Popen([str(config.PLAMENU_DIR / "e2e/.venv/bin/python"),
                str(config.PLAMENU_DIR / "e2e/tools/tracker_fault_proxy.py"), "--listen", "8422", "--upstream", str(upstream),
                "--control", control.rsplit(":", 1)[1], "--bandwidth", "1048576", "--latency-ms", "250",
                "--result", str(evidence / "proxy.json")], env=environment, stdout=proxy_log, stderr=subprocess.STDOUT)
            def ready():
                try:
                    return requests.post(control + "/status", headers={"X-AP09-Control": token}, timeout=1).ok
                except requests.RequestException:
                    return False
            wait_for(ready, desc="owned native HTTP body bridge is ready", timeout=10)
            origin._launch(120, append_log=True)
            origin.apply_suite_settings()
            user, api = origin.user_api("peerbytesviewer")
            title = f"Matched eight-viewer emission {label}"
            movie = peertube.make_sample_mp4(evidence / "source.mp4", seconds=180, size="640x480")
            if product == "native":
                channel = api.post("/api/v1/video_channels", username=unique("bytefilms")[:30], display_name="Matched films")
                handle = f"{channel['username']}@{origin.domain}"
                pt.post("/api/v1/users/me/subscriptions", uri=handle)
                subscribed = True
                wait_for(lambda: origin.db.follower_count(channel["username"]) > 0, desc="actual PeerTube follows matched film creator")
                media, browser = _submit(origin, user, channel, movie, title)
                browser.close()
                wait_for(lambda: origin.db_value("SELECT state='ready' FROM video_drafts WHERE media_id=%s", media),
                    desc="native film's complete checksum ladder is ready", timeout=240)
                status = api.post("/api/v1/videos", channel_id=channel["id"], media_id=str(media), title=title, visibility="public")
                remote = wait_for(lambda: next((v for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100,
                    sort="-publishedAt")["data"] if v["name"] == title), None), desc="actual PeerTube receives matched native film")
            else:
                root = api.resolve_account(f"root@{config.PEERTUBE_DOMAIN}")
                api.follow(root["id"])
                remote = pt.upload_video(movie, title)
                pt.wait_for_hls(remote["uuid"])
                def transcodes_done():
                    for state in ("active", "waiting", "waiting-children", "prioritized", "delayed"):
                        jobs = pt.get(f"/api/v1/jobs/{state}", jobType="video-transcoding", count=100)
                        assert jobs["total"] <= 100
                        if any(job["data"].get("videoUUID") == remote["uuid"] for job in jobs["data"]):
                            return False
                    return True
                wait_for(transcodes_done, desc="actual source files stop rotating", timeout=240, interval=3)
                status = wait_for(lambda: api.resolve_status(pt.ap_id(remote["uuid"])), desc="native service imports actual PeerTube source")
            assert status["media_attachments"][0]["hls"]["p2p"]
            manifest = evidence / "viewers.json"
            manifest.write_text(json.dumps({"origin": product, "local": f"{origin.url}/web/statuses/{status['id']}"}))
        with step("matched eight fresh-context 240p viewers, seed-inclusive real HTTP emissions and accepted peer bytes"):
            browser_log = (evidence / "browser.log").open("w")
            logs.append(browser_log)
            observer = subprocess.Popen(["node", str(config.PLAMENU_DIR / "accessibility/verify-peer-origin-bytes.mjs"),
                str(manifest), control, str(evidence)], env=environment, stdout=browser_log, stderr=subprocess.STDOUT)
            began = time.monotonic()
            previous_cpu, cpu_seconds = {}, 0
            while observer.poll() is None:
                assert time.monotonic() - began <= 360, "matched delivery measurement exceeded finite deadline"
                assert origin._proc.poll() is None and proxy.poll() is None
                processes = [p for pid in {origin._proc.pid, *_children(origin._proc.pid)} if (p := _process(pid)) is not None]
                for process in processes:
                    identity = process["identity"]
                    if identity in previous_cpu:
                        cpu_seconds += max(0, process["cpu"] - previous_cpu[identity])
                    previous_cpu[identity] = process["cpu"]
                samples.append({"seconds": time.monotonic() - began, "rss": sum(p["rss"] for p in processes)})
                time.sleep(1)
            elapsed = time.monotonic() - began
            resources = {"candidate_sha256": candidate, "origin": product, "seconds": elapsed,
                "mean_service_cpu_cores": cpu_seconds / elapsed, "max_service_rss": max(s["rss"] for s in samples),
                "scope": "native service and children; browser, bridge and actual PeerTube excluded",
                "budgets": {"mean_service_cpu_cores": 3, "max_service_rss": 768 * 1024**2}, "samples": samples}
            (evidence / "resource-result.json").write_text(json.dumps(resources, indent=2))
            assert observer.returncode == 0, "inspect retained real response-body and decoded arm observations"
            assert resources["mean_service_cpu_cores"] <= 3 and resources["max_service_rss"] <= 768 * 1024**2
            print(f"Protected seed-inclusive native HTTP body receipt: {evidence}")
    finally:
        if observer is not None and observer.poll() is None:
            _stop_encoder(observer)
        if samples:
            (evidence / "resource-progress.json").write_text(json.dumps(samples, indent=2))
        try:
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
            if product == "peertube" and remote is not None:
                pt.delete_video(remote["id"])
        finally:
            try:
                if origin._dir is not None:
                    log = Path(origin._dir.name) / "server.log"
                    if log.exists():
                        (evidence / "server.log").write_bytes(log.read_bytes())
                origin.stop()
            finally:
                if proxy is not None:
                    _stop_encoder(proxy)
                for log in logs:
                    log.close()
