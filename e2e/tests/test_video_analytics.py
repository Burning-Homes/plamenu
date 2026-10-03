"""Real PeerTube VOD ranges and native live bodies, with private opt-in totals.

Only a uniquely named disposable instance/database/media tree is modified.
No viewer event log, credentials, IPs or cookies are retained in the manifest.
"""

import hashlib
import json
import re
import socket
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from test_video_backup_restore import _csrf, _stop_process


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="actual PeerTube imports a native film and offers its playable origin range",
)
def test_native_delivery_analytics_with_real_peer_ranges_and_live_encoder():
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable peer port is occupied"
        )
    label = unique("analytics").replace("-", "_")
    peer = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    subscribed = False
    handle = publisher = None
    api = channel = None
    evidence_root = config.PLAMENU_DIR / "target/e2e/video-analytics"
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=evidence_root))
    evidence.chmod(0o700)
    try:
        with step("create a native film with analytics disabled by default"):
            peer.start()
            # Allocate only this disposable instance's RTMP listener. Never
            # restart, reconfigure or publish to the standing development server.
            with socket.socket() as allocator:
                allocator.bind(("127.0.0.1", 0))
                rtmp_port = allocator.getsockname()[1]
            _stop_process(peer)
            configuration = peer._config_path.read_text()
            assert 'live_ingest_bind = "127.0.0.1:0"' in configuration
            peer._config_path.write_text(
                configuration.replace(
                    'live_ingest_bind = "127.0.0.1:0"',
                    f'live_ingest_bind = "127.0.0.1:{rtmp_port}"',
                )
            )
            peer._launch(120, append_log=True)
            peer.apply_suite_settings()
            user, api = peer.user_api("deliveryauthor")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("deliveries")[:30],
                display_name="Delivery test films",
            )
            handle = f"{channel['username']}@{peer.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: peer.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube follows the opted-out creator",
            )
            login = browser.post(
                f"{peer.url}/login",
                data={"email": user.email, "password": user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303
            csrf = _csrf(browser.get(f"{peer.url}/videos", timeout=30).text)
            sample = peertube.make_sample_mp4(
                evidence / "sample.mp4", seconds=8, size="640x480"
            )
            title = f"Analytics {label}"
            fields = {
                "csrf": csrf,
                "channel": channel["id"],
                "title": title,
                "description": "Moving test pattern and steady tone",
                "visibility": "public",
                "language": "en",
                "alt": "Moving test pattern",
                "op": "review",
            }
            with sample.open("rb") as stream:
                uploaded = browser.post(
                    f"{peer.url}/web/videos",
                    files=[
                        *((key, (None, str(value))) for key, value in fields.items()),
                        ("file", ("sample.mp4", stream, "video/mp4")),
                    ],
                    allow_redirects=False,
                    timeout=90,
                )
            assert uploaded.status_code == 303, uploaded.status_code
            media = urlsplit(
                urljoin(peer.url, uploaded.headers["Location"])
            ).path.rsplit("/", 1)[1]
            wait_for(
                lambda: peer.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                    int(media),
                ),
                desc="actual native film processing",
                timeout=180,
            )
            film = api.post(
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
                desc="actual PeerTube imports the film",
            )
            file_url = pt.video(remote["uuid"])["files"][0]["fileUrl"]
            assert urlsplit(file_url).hostname == peer.domain
            first = requests.get(
                file_url, headers={"Range": "bytes=0-31"}, verify=False, timeout=30
            )
            assert first.status_code == 206 and len(first.content) == 32
            endpoint = f"/api/v1/video_channels/{channel['id']}/analytics"
            assert api.get(endpoint)["daily"] == []

        with step(
            "explicit opt-in counts real PeerTube-offered body bytes but no viewer events"
        ):
            api._request("PUT", endpoint, json={"enabled": True, "retention_days": 7})
            second = requests.get(
                file_url, headers={"Range": "bytes=0-31"}, verify=False, timeout=30
            )
            assert second.status_code == 206 and second.content == first.content
            rows = wait_for(
                lambda: api.get(endpoint)["daily"],
                desc="batched native delivery totals",
                timeout=45,
            )
            assert sum(row["body_bytes"] for row in rows) >= 32
            assert all(
                set(row)
                == {
                    "media_id",
                    "status_id",
                    "title",
                    "day",
                    "lane",
                    "requests",
                    "body_bytes",
                }
                for row in rows
            )
            assert all(re.fullmatch(r"\d{4}-\d{2}-\d{2}", row["day"]) for row in rows)
            anonymous = requests.get(f"{peer.url}{endpoint}", verify=False, timeout=30)
            assert (
                anonymous.status_code == 401
                and anonymous.headers["Cache-Control"] == "private, no-store"
            )

        with step(
            "an actual native encoder and already-connected progressive body produce live aggregates"
        ):
            created = browser.post(
                f"{peer.url}/web/videos/live",
                data={
                    "csrf": csrf,
                    "channel": channel["id"],
                    "title": f"Live analytics {label}",
                    "description": "Camera-like pattern and steady tone",
                    "visibility": "public",
                    "language": "en",
                    "dvr_minutes": "5",
                    "latency": "low",
                },
                timeout=30,
            )
            assert created.status_code == 200, created.status_code
            status = re.search(r"/videos/live/(\d+)", created.text).group(1)
            inputs = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            assert len(inputs) == 3
            server, key, _https = inputs
            assert server == f"rtmp://127.0.0.1:{rtmp_port}/live"
            live = api.get(f"/api/v1/statuses/{status}")["media_attachments"][0]["id"]
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
                    "testsrc2=size=320x180:rate=10",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-tune",
                    "zerolatency",
                    "-g",
                    "10",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-t",
                    "90",
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
                desc="disposable encoder reaches on-air state",
            )

            def first_viewer():
                with requests.get(
                    f"{peer.url}/media/live/{live}/stream.mp4",
                    stream=True,
                    verify=False,
                    timeout=(10, 30),
                ) as response:
                    assert response.status_code == 200, (
                        f"first progressive request: {response.status_code}"
                    )
                    initialization = next(response.iter_content(4096))
                    assert b"ftyp" in initialization[:32]
                    return initialization

            # Concurrent first viewers must share the *pending* startup as
            # well as the running remux, without waiting for an HLS playlist.
            with ThreadPoolExecutor(max_workers=4) as viewers:
                initializations = list(viewers.map(lambda _: first_viewer(), range(4)))
            # Tokio may spawn on any worker thread; Linux records children
            # against that task, not necessarily the process's main task.
            children = set()
            for task in Path(f"/proc/{peer._proc.pid}/task").iterdir():
                try:
                    children.update((task / "children").read_text().split())
                except FileNotFoundError:
                    continue
            remuxes = []
            for pid in children:
                try:
                    command = Path(f"/proc/{pid}/cmdline").read_bytes()
                except FileNotFoundError:
                    continue
                if f"/media/hls/{live}/master.m3u8".encode() in command:
                    remuxes.append(pid)
            assert len(remuxes) == 1, (
                "concurrent startup must retain exactly one shared remux child"
            )
            live_rows = wait_for(
                lambda: [
                    row for row in api.get(endpoint)["daily"] if row["lane"] == "live"
                ],
                desc="actual progressive live delivery is accounted",
                timeout=45,
            )
            assert sum(row["body_bytes"] for row in live_rows) >= sum(
                map(len, initializations)
            )
            publisher.terminate()
            publisher.wait(timeout=15)
            publisher = None

        with step(
            "erasure preserves opt-in, disabling clears retention and leaves playback compatible"
        ):
            manifest = {
                "binary_sha256": hashlib.sha256(
                    ephemeral.binary_path().read_bytes()
                ).hexdigest(),
                "vod_body_bytes": sum(row["body_bytes"] for row in rows),
                "live_body_bytes": sum(row["body_bytes"] for row in live_rows),
                "concurrent_first_viewers": len(initializations),
                "shared_remux_children": len(remuxes),
                "native_range_sha256": hashlib.sha256(second.content).hexdigest(),
                "privacy_columns": peer.db.conn.execute(
                    "SELECT column_name FROM information_schema.columns "
                    "WHERE table_name='video_delivery_daily' ORDER BY column_name"
                ).fetchall(),
            }
            api.delete(endpoint)
            assert (
                api.get(endpoint)["daily"] == []
                and api.get(endpoint)["policy"]["enabled"]
            )
            api._request("PUT", endpoint, json={"enabled": False, "retention_days": 7})
            assert (
                requests.get(
                    file_url, headers={"Range": "bytes=0-31"}, verify=False, timeout=30
                ).content
                == first.content
            )
            assert api.get(endpoint)["daily"] == []
            api.delete(f"/api/v1/statuses/{film['id']}")
            (evidence / "manifest.json").write_text(json.dumps(manifest, indent=2))
    finally:
        if publisher is not None:
            publisher.terminate()
            try:
                publisher.wait(timeout=15)
            except subprocess.TimeoutExpired:
                publisher.kill()
                publisher.wait(timeout=10)
        try:
            if (
                api is not None
                and channel is not None
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
            browser.close()
            if peer._dir is not None:
                log = Path(peer._dir.name) / "server.log"
                if log.exists():
                    retained_log = evidence / "server.log"
                    retained_log.write_text(log.read_text())
                    retained_log.chmod(0o600)
            peer.stop()
