"""Real first-party camera/screen authoring and decoded glass-clock budgets.

Only the browser's hardware source is synthetic. The first-party controls,
MediaRecorder/WebSocket, paired native encoder, and actual PeerTube are real.
"""
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import tempfile
import time
import tomllib

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _stop_process
from tests.test_video_delivery_scale import _children, _process, _tree_bytes
from tests.test_video_live_backup_restore import _stop_encoder
from tests.test_video_offload import _freeze


@pytest.mark.skipif(os.environ.get("AP09_CAPTURE_VERIFY") != "1", reason="owned real capture/performance drill")
@pytest.mark.federation(direction="outbound", peer="peertube",
    one_way_reason="actual PeerTube decodes both first-party browser capture generations")
def test_first_party_camera_and_screen_capture_decoded_budgets(monkeypatch):
    assert peertube.reachable(), "actual PeerTube is required"
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("capturebudget").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-capture-budget"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(domain=config.PLAMENU2_DOMAIN, bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}")
    session = requests.Session()
    session.verify = False
    observer = channel = api = pt = log = None
    subscribed = False
    samples, generations = [], {}
    try:
        with step("paired native storage and an owned permanent broadcast followed by actual PeerTube"):
            origin.start()
            candidate = _freeze(origin, evidence, monkeypatch)
            _stop_process(origin)
            replica = evidence / "replica:paired|capture"
            replica.mkdir(mode=0o700)
            settings = tomllib.loads(origin._config_path.read_text())
            primary = Path(settings["media_dir"])
            origin._config_path.write_text(origin._config_path.read_text() +
                f'\n[media_mirror]\ndirectory={json.dumps(str(replica))}\nbandwidth_mib_per_second=32\nmin_free_gib=1\n')
            origin._config_path.chmod(0o600)
            origin._launch(120, append_log=True)
            origin.apply_suite_settings()
            user, api = origin.user_api("capturecaster")
            channel = api.post("/api/v1/video_channels", username=unique("capturelive")[:30], display_name="Browser capture budgets")
            pt = peertube.root()
            handle = f"{channel['username']}@{origin.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(lambda: origin.db.follower_count(channel["username"]) > 0, desc="actual PeerTube follows capture creator")
            assert session.post(origin.url + "/login", data={"email": user.email, "password": user.password},
                allow_redirects=False, timeout=30).status_code == 303
            title = f"First-party camera and screen {label}"
            created = session.post(origin.url + "/web/videos/live", data={
                "csrf": _csrf(session.get(origin.url + "/videos", timeout=30).text), "channel": channel["id"],
                "title": title, "description": "Synthetic hardware source; real capture controls and pixel clock",
                "visibility": "public", "permanent": "yes", "latency": "balanced", "dvr_minutes": "5"}, timeout=30)
            assert created.status_code == 200
            status = re.search(r'/videos/live/(\d+)', created.text).group(1)
            media = int(api.get_status(status)["media_attachments"][0]["id"])
            manifest = evidence / "viewers.json"
            manifest.write_text(json.dumps({"manage": f"{origin.url}/videos/live/{status}",
                "watch": f"{origin.url}/web/statuses/{status}", "status": status}))
            log = (evidence / "browser.log").open("w")
            observer = subprocess.Popen(["node", str(config.PLAMENU_DIR / "accessibility/verify-browser-capture-budget.mjs"),
                str(manifest), str(evidence)], stdout=log, stderr=subprocess.STDOUT, env={**os.environ,
                    "AP09_CAPTURE_USER": user.email, "AP09_CAPTURE_PASSWORD": user.password,
                    "PEERTUBE_E2E_USER": peertube.ROOT_USER, "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS})

        with step("two real browser capture generations within declared decode and service budgets"):
            started = time.monotonic()
            previous_cpu, cpu_seconds = {}, 0
            while observer.poll() is None:
                assert time.monotonic() - started < 360, "capture drill exceeded finite deadline"
                assert origin._proc.poll() is None
                report = evidence / "capture-browser.json"
                stage = json.loads(report.read_text()).get("stage") if report.exists() else None
                for kind in ("camera", "screen"):
                    if stage == f"{kind}-on-air" and kind not in generations:
                        generation = origin.db_value("SELECT session_id FROM native_live_sessions WHERE media_id=%s AND ended_at IS NULL", media)
                        assert generation and generation not in generations.values()
                        durable = replica / "native-live" / str(media) / generation
                        wait_for(lambda: (durable / "index.durable.m3u8").exists(), desc=f"{kind} capture has durable paired checkpoint", timeout=30)
                        remote = wait_for(lambda: next((video for video in pt.get("/api/v1/users/me/subscriptions/videos", count=100,
                            sort="-publishedAt")["data"] if video["name"] == title and video["isLive"]), None),
                            desc=f"actual PeerTube receives {kind} capture", timeout=60)
                        generations[kind] = generation
                        (evidence / f"remote-{kind}.json").write_text(json.dumps({"url": f"{config.PEERTUBE_URL}/w/{remote['uuid']}"}))
                children = _children(origin._proc.pid)
                processes = [p for pid in {origin._proc.pid, *children} if (p := _process(pid)) is not None]
                for process in processes:
                    identity = process["identity"]
                    if identity in previous_cpu:
                        cpu_seconds += max(0, process["cpu"] - previous_cpu[identity])
                    previous_cpu[identity] = process["cpu"]
                encoders = 0
                for pid in children:
                    try:
                        encoders += Path(f"/proc/{pid}/comm").read_text().strip() == "ffmpeg"
                    except FileNotFoundError:
                        pass
                samples.append({"seconds": time.monotonic() - started, "service_rss": sum(p["rss"] for p in processes),
                    "ffmpeg_children": encoders, "primary_bytes": _tree_bytes(primary), "replica_bytes": _tree_bytes(replica)})
                assert encoders <= 4, "capture must not encode per viewer"
                time.sleep(1)
            elapsed = time.monotonic() - started
            result = {"candidate_sha256": candidate, "duration_seconds": elapsed, "generations": generations,
                "mean_service_cpu_cores": cpu_seconds / elapsed, "max_service_rss": max(s["service_rss"] for s in samples),
                "max_ffmpeg_children": max(s["ffmpeg_children"] for s in samples), "samples": samples,
                "measurement_scope": "origin and its children; browsers and synthetic sources excluded",
                "budgets": {"mean_service_cpu_cores": 3, "max_service_rss": 768 * 1024**2,
                    "max_ffmpeg_children": 4, "browser_deadline_seconds": 360}}
            (evidence / "resource-result.json").write_text(json.dumps(result, indent=2))
            assert observer.returncode == 0, "inspect protected browser log and capture-browser.json"
            assert len(generations) == 2
            assert result["max_service_rss"] <= result["budgets"]["max_service_rss"]
            assert result["mean_service_cpu_cores"] <= result["budgets"]["mean_service_cpu_cores"]
            wait_for(lambda: origin.db_value("SELECT live_state='waiting' FROM media_attachments WHERE id=%s", media),
                desc="actual capture stop returns permanent broadcast to waiting", timeout=15)
            for kind, generation in generations.items():
                def replay_ready():
                    return origin.db_value("SELECT m.status_id FROM native_live_sessions s JOIN media_attachments m ON m.id=s.replay_media_id "
                        "WHERE s.session_id=%s AND m.processing='complete'", generation)
                replay_status = wait_for(replay_ready, desc=f"{kind} captured audio/video replay completes", timeout=240)
                replay = api.get_status(replay_status)["media_attachments"][0]
                movie = evidence / f"{kind}-replay.mp4"
                response = requests.get(replay["url"], verify=False, timeout=30)
                response.raise_for_status()
                movie.write_bytes(response.content)
                subprocess.run(["ffmpeg", "-nostdin", "-v", "error", "-xerror", "-i", str(movie),
                    "-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"], check=True, capture_output=True, timeout=90)
            print(f"Protected browser capture budget evidence: {evidence}")
    finally:
        if samples:
            (evidence / "resource-progress.json").write_text(json.dumps(samples, indent=2))
        if observer is not None and observer.poll() is None:
            _stop_encoder(observer)
        if log is not None:
            log.close()
        try:
            if origin._proc is not None and origin._proc.poll() is None and channel is not None:
                api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=channel["username"])
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            if origin._dir is not None:
                server_log = Path(origin._dir.name) / "server.log"
                if server_log.exists():
                    (evidence / "origin.log").write_text(server_log.read_text())
            origin.stop()
            session.close()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
