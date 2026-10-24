"""Actual ten-minute native/PT film and cold JS/no-JS constrained TCP seeking.

Owned bridge/server, one finite predeclared profile, retained source and actual
decoder snapshots. No global traffic shaping, synthetic player or fake ranges.
"""

import json
import os
import secrets
import socket
import subprocess
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _digest, _stop_process
from tests.test_video_live_backup_restore import _stop_encoder
from tests.test_video_live_concurrency import _port
from tests.test_video_offload import _freeze, _submit


@pytest.mark.capability("AP09_COLD_VERIFY")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="actual PeerTube accepts the native ten-minute quality ladder",
)
def test_cold_far_seek_and_constrained_decoders_have_actual_origin_byte_receipts(
    monkeypatch,
):
    assert peertube.reachable()
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("coldfilm").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-cold"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    bridge = channel = api = pt = None
    subscribed = False
    logs = []
    token = secrets.token_hex(32)
    environment = {**os.environ, "AP09_PROXY_TOKEN": token}
    try:
        with step(
            "owned canonical HTTPS origin uses actual bounded TCP network fixture"
        ):
            origin.start(suite_settings=True)
            candidate = _freeze(origin, evidence, monkeypatch)
            _stop_process(origin)
            upstream, control_port = _port(), _port()
            origin._config_path.write_text(
                origin._config_path.read_text().replace(
                    'bind = "0.0.0.0:8422"', f'bind = "127.0.0.1:{upstream}"'
                )
            )
            origin._config_path.chmod(0o600)
            control = f"http://127.0.0.1:{control_port}"
            bridge_log = (evidence / "bridge.log").open("w")
            logs.append(bridge_log)
            bridge = subprocess.Popen(
                [
                    str(config.PLAMENU_DIR / "e2e/.venv/bin/python"),
                    str(config.PLAMENU_DIR / "e2e/tools/tracker_fault_proxy.py"),
                    "--listen",
                    "8422",
                    "--upstream",
                    str(upstream),
                    "--control",
                    str(control_port),
                    "--bandwidth",
                    "1048576",
                    "--latency-ms",
                    "40",
                    "--result",
                    str(evidence / "bridge.json"),
                ],
                env=environment,
                stdout=bridge_log,
                stderr=subprocess.STDOUT,
            )

            def ready():
                try:
                    return requests.post(
                        control + "/status",
                        headers={"X-AP09-Control": token},
                        timeout=1,
                    ).ok
                except requests.RequestException:
                    return False

            wait_for(ready, desc="actual owned TCP bridge listens", timeout=10)
            origin._launch(120, append_log=True)
            user, api = origin.user_api("coldcaster")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("coldfilms")[:30],
                display_name="Cold films",
            )
            pt = peertube.root()
            handle = f"{channel['username']}@{origin.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: origin.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube follows before film",
            )
        with step(
            "real ten-minute moving film cannot already buffer its 570-second seek"
        ):
            source = evidence / "ten-minute-source.mp4"
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x360:rate=15",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440",
                    "-t",
                    "600",
                    "-c:v",
                    "libx264",
                    "-threads",
                    "2",
                    "-preset",
                    "veryfast",
                    "-b:v",
                    "2500k",
                    "-minrate",
                    "2500k",
                    "-maxrate",
                    "2500k",
                    "-bufsize",
                    "5000k",
                    "-x264-params",
                    "nal-hrd=cbr:force-cfr=1",
                    "-g",
                    "60",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "128k",
                    "-movflags",
                    "+faststart",
                    str(source),
                ],
                check=True,
                capture_output=True,
                timeout=180,
            )
            mid, _ = _submit(
                origin, user, channel, source, f"Cold ten-minute native film {label}"
            )
            wait_for(
                lambda: origin.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s", mid
                ),
                desc="finite native quality ladder is ready",
                timeout=240,
            )
            published = api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=str(mid),
                title=f"Cold ten-minute native film {label}",
                visibility="public",
            )
            remote = wait_for(
                lambda: next(
                    (
                        v
                        for v in pt.get(
                            "/api/v1/users/me/subscriptions/videos",
                            count=100,
                            sort="-publishedAt",
                        )["data"]
                        if v["name"] == f"Cold ten-minute native film {label}"
                    ),
                    None,
                ),
                desc="actual PeerTube imports ten-minute native ladder",
            )
            details = pt.video(remote["uuid"])
            assert details["duration"] >= 599 and details["streamingPlaylists"]
            manifest = evidence / "manifest.json"
            manifest.write_text(
                json.dumps(
                    {
                        "serverOnlyEmbed": f"{origin.url}/web/statuses/{published['id']}/embed?p2p=0"
                    }
                )
            )
        with step(
            "four fresh real contexts meet predeclared cold and 8-Mibit/80-ms budgets"
        ):
            browser_log = (evidence / "browser.log").open("w")
            logs.append(browser_log)
            subprocess.run(
                [
                    "node",
                    str(
                        config.PLAMENU_DIR
                        / "accessibility/verify-cold-video-budget.mjs"
                    ),
                    str(manifest),
                    control,
                    str(evidence),
                ],
                env=environment,
                stdout=browser_log,
                stderr=subprocess.STDOUT,
                check=True,
                timeout=240,
            )
            result = {
                "candidate_sha256": candidate,
                "source_sha256": _digest(source),
                "source_bytes": source.stat().st_size,
                "peer_uuid": remote["uuid"],
                "peer_duration": details["duration"],
                "browser": json.loads((evidence / "cold-browser.json").read_text()),
            }
            (evidence / "result.json").write_text(json.dumps(result, indent=2))
            print(f"Protected real cold/constrained evidence: {evidence}")
    finally:
        try:
            if (
                origin._proc is not None
                and origin._proc.poll() is None
                and channel is not None
            ):
                api.post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            if origin._dir is not None:
                log = Path(origin._dir.name) / "server.log"
                if log.exists():
                    (evidence / "origin.log").write_text(log.read_text())
            origin.stop()
            _stop_encoder(bridge)
            for log in logs:
                log.close()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
