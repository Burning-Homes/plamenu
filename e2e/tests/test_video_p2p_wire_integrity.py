"""Both-origin, actual RTC corrupted/stale media with unmodified validators."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile

import pytest
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_offload import _freeze, _submit


@pytest.mark.skipif(os.environ.get("AP09_P2P_FAULT_VERIFY") != "1", reason="owned real RTC integrity/fallback drill")
@pytest.mark.federation(direction="both", peer="peertube")
@pytest.mark.parametrize("product", ["native", "peertube"])
def test_both_origin_corrupt_and_stale_peer_bytes_are_rejected_before_decode(product, monkeypatch):
    assert peertube.reachable()
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("peerintegrity").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-peer-integrity"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-{product}-", dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(domain=config.PLAMENU2_DOMAIN, bind="0.0.0.0:8422", db_name=f"plamenu_ap09_{label}")
    pt = peertube.root()
    channel = api = remote_video = None
    subscribed = False
    try:
        with step("frozen owned native instance and an actual checksum-backed film from the selected origin"):
            origin.start()
            candidate = _freeze(origin, evidence, monkeypatch)
            origin.apply_suite_settings()
            user, api = origin.user_api("peerchecker")
            movie = peertube.make_sample_mp4(evidence / "source.mp4", seconds=180, size="640x480")
            title = f"Actual {product} RTC integrity {label}"
            if product == "native":
                channel = api.post("/api/v1/video_channels", username=unique("integrityfilms")[:30], display_name="Integrity films")
                handle = f"{channel['username']}@{origin.domain}"
                pt.post("/api/v1/users/me/subscriptions", uri=handle)
                subscribed = True
                wait_for(lambda: origin.db.follower_count(channel["username"]) > 0, desc="actual PeerTube follows owned creator")
                media, browser = _submit(origin, user, channel, movie, title)
                browser.close()
                wait_for(lambda: origin.db_value("SELECT state='ready' FROM video_drafts WHERE media_id=%s", media),
                    desc="native checksum-backed ladder is ready", timeout=240)
                status = api.post("/api/v1/videos", channel_id=channel["id"], media_id=str(media), title=title, visibility="public")
                remote_video = wait_for(lambda: next((v for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100,
                    sort="-publishedAt")["data"] if v["name"] == title), None), desc="actual PeerTube receives native film")
            else:
                root = api.resolve_account(f"root@{config.PEERTUBE_DOMAIN}")
                api.follow(root["id"])
                remote_video = pt.upload_video(movie, title)
                pt.wait_for_hls(remote_video["uuid"])
                def transcodes_done():
                    for state in ("active", "waiting", "waiting-children", "prioritized", "delayed"):
                        jobs = pt.get(f"/api/v1/jobs/{state}", jobType="video-transcoding", count=100)
                        assert jobs["total"] <= 100
                        if any(job["data"].get("videoUUID") == remote_video["uuid"] for job in jobs["data"]):
                            return False
                    return True
                wait_for(transcodes_done, desc="owned PeerTube rungs finish before consumer import", timeout=240, interval=3)
                status = wait_for(lambda: api.resolve_status(pt.ap_id(remote_video["uuid"])), desc="native consumer imports actual PeerTube film")
            attachment = status["media_attachments"][0]
            assert attachment["hls"]["p2p"]
            manifest = evidence / "viewers.json"
            manifest.write_text(json.dumps({"origin": product, "local": f"{origin.url}/web/statuses/{status['id']}",
                "remote": f"{config.PEERTUBE_URL}/w/{remote_video['uuid']}"}))
        with step("actual cross-product peer bytes, corrupted bytes, stale bytes and real HTTP decoder fallback"):
            with (evidence / "browser.log").open("w") as output:
                observer = subprocess.run(["node", str(config.PLAMENU_DIR / "accessibility/verify-p2p-wire-integrity.mjs"),
                    str(manifest), str(evidence)], stdout=output, stderr=subprocess.STDOUT, timeout=360,
                    env={**os.environ, "PEERTUBE_E2E_USER": peertube.ROOT_USER, "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS})
            assert observer.returncode == 0, "inspect protected actual RTC/decoder observations and browser log"
            (evidence / "result.json").write_text(json.dumps({"candidate_sha256": candidate, "origin": product,
                "browsers": json.loads((evidence / "peer-integrity-browser.json").read_text())}, indent=2))
            print(f"Protected both-origin RTC integrity evidence: {evidence}")
    finally:
        try:
            if origin._proc is not None and origin._proc.poll() is None and channel is not None:
                api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=channel["username"])
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
            if product == "peertube" and remote_video is not None:
                pt.delete_video(remote_video["id"])
        finally:
            if origin._dir is not None:
                log = Path(origin._dir.name) / "server.log"
                if log.exists():
                    (evidence / "origin.log").write_text(log.read_text())
            origin.stop()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
