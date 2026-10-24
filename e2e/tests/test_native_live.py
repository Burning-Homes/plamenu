"""Native RTMP/HTTPS-FLV -> local HLS/MP4 -> PeerTube lifecycle."""

import asyncio
import hashlib
import json
import os
import re
import ssl
import subprocess
import tempfile
import threading
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
import requests
import websockets
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.db import Db
from plamenu_e2e.steps import log, step, wait_for


def rtmps_tls_args():
    ca = os.environ.get("PLAMENU_RTMPS_CA")
    return ["-cafile", ca, "-tls_verify", "1"] if ca else ["-tls_verify", "0"]


@pytest.mark.requires_peer("peertube")
@pytest.mark.parametrize(
    "transport",
    [
        "rtmp",
        "https",
        pytest.param(
            "rtmps",
            marks=pytest.mark.capability("PLAMENU_RTMPS"),
        ),
    ],
)
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    reverse_of="test_peertube_live_federates_and_plays_in_every_client",
)
def test_native_live_hls_and_peertube(
    plamenu_api: Api, plamenu_user, db: Db, transport: str
):
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("nativelive")[:30]
    title = f"Native live {name}"
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Native live tests"
    )
    status_id = None
    publisher = None
    try:
        with step("PeerTube follows the Plamenu video channel"):
            pt.post(
                "/api/v1/users/me/subscriptions",
                uri=f"{name}@{config.PLAMENU_DOMAIN}",
            )

        with step(
            "a no-JS form publishes a waiting Video and shows its ingest key once"
        ):
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303, login.text[:500]
            form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
            form.raise_for_status()
            csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
            planned = (datetime.now(UTC) + timedelta(hours=2)).strftime(
                "%Y-%m-%dT%H:%M"
            )
            next_planned = (datetime.now(UTC) + timedelta(hours=4)).strftime(
                "%Y-%m-%dT%H:%M"
            )
            created = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live",
                data={
                    "csrf": csrf,
                    "channel": channel["id"],
                    "title": title,
                    "description": "A live test pattern with an audible 440 Hz tone.",
                    "language": "en",
                    "visibility": "public",
                    "category": "15",
                    "licence": "2",
                    "spoiler_text": "Flashing test imagery",
                    "dvr_minutes": "5",
                    "latency": "low",
                    "scheduled_at": planned,
                    "scheduled_starts": next_planned,
                },
                timeout=30,
            )
            assert created.status_code == 200, created.text[:700]
            status_id = re.search(r"/videos/live/(\d+)", created.text).group(1)
            inputs = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            assert len(inputs) == (
                4 if os.environ.get("PLAMENU_RTMPS") == "1" else 3
            ), created.text[:1000]
            if len(inputs) == 4:
                server, secure_server, key, https_ingest = inputs
                assert secure_server.startswith("rtmps://")
            else:
                server, key, https_ingest = inputs
                secure_server = None
            assert server.startswith("rtmp://127.0.0.1:") and server.endswith("/live")
            assert https_ingest == f"{config.PLAMENU_URL}/live/ingest"
            assert len(key) == 43
            assert (
                key
                not in browser.get(
                    f"{config.PLAMENU_URL}/videos/live/{status_id}", timeout=30
                ).text
            )

            waiting_status = plamenu_api.get(f"/api/v1/statuses/{status_id}")
            assert (
                waiting_status["sensitive"]
                and waiting_status["spoiler_text"] == "Flashing test imagery"
            )
            waiting = waiting_status["media_attachments"][0]
            media_id = waiting["id"]
            assert waiting["live"]["state"] == "waiting"
            assert waiting["live"]["permanent"] is False
            assert waiting["live"]["dvr_window_seconds"] == 300
            assert waiting["live"]["latency_mode"] == 3
            assert waiting["live"]["scheduled_at"].startswith(planned)
            assert [date[:16] for date in waiting["live"]["schedules"]] == [
                planned,
                next_planned,
            ]
            public_page = browser.get(
                f"{config.PLAMENU_URL}/web/statuses/{status_id}", timeout=30
            )
            assert "Planned start:" in public_page.text and "<time" in public_page.text
            ap = plamenu_api.ap_get(
                waiting_status["uri"].removeprefix(config.PLAMENU_URL)
            )
            assert ap["schedules"][0]["startDate"].startswith(planned)
            assert (
                ap["category"]["identifier"] == "15"
                and ap["licence"]["identifier"] == "2"
            )
            assert (
                ap["licence"]["url"]
                == "https://creativecommons.org/licenses/by-sa/4.0/"
            )
            assert ap["sensitive"] and ap["summary"] == "Flashing test imagery"
            assert ap["schedules"][1]["startDate"].startswith(next_planned)
            assert waiting["url"] is None
            assert (
                requests.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8",
                    verify=False,
                    timeout=20,
                ).status_code
                == 404
            )
            if transport == "https":
                assert (
                    requests.post(
                        https_ingest, data=b"", verify=False, timeout=20
                    ).status_code
                    == 401
                )
                assert (
                    requests.post(
                        "http://127.0.0.1:8420/live/ingest",
                        headers={"Authorization": f"Bearer {key}"},
                        data=b"",
                        timeout=20,
                    ).status_code
                    == 403
                ), "direct HTTP ingest without TLS proxy metadata is refused"

        with step("the waiting Video retains a stable PeerTube-compatible identity"):
            uuid = db.conn.execute(
                "SELECT uuid FROM video_publications WHERE status_id = %s",
                (int(status_id),),
            ).fetchone()[0]
            assert re.fullmatch(r"[0-9a-f-]{36}", uuid)
            # The authenticated PeerTube live endpoint may expose the waiting
            # metadata even though public video listings hide waiting lives.
            remote_planned = wait_for(
                lambda: _remote_live_schedule(pt, uuid, planned),
                desc="PeerTube imports the planned start",
                timeout=60,
            )
            assert remote_planned["schedules"][0]["startAt"].startswith(planned)
            assert remote_planned["schedules"][1]["startAt"].startswith(next_planned)

        def output_args(publish_key: str):
            if transport == "rtmp":
                return ["-f", "flv", f"{server}/{publish_key}"]
            if transport == "rtmps":
                return [
                    "-f",
                    "flv",
                    *rtmps_tls_args(),
                    f"{secure_server}/{publish_key}",
                ]
            return [
                "-f",
                "flv",
                "-method",
                "POST",
                "-headers",
                f"Authorization: Bearer {publish_key}\r\n",
                "-tls_verify",
                "0",
                https_ingest,
            ]

        with step(
            f"{transport} camera-like A/V becomes bounded local HLS and no-JS MP4"
        ):
            if transport in ("rtmp", "rtmps"):
                rejected = subprocess.run(
                    [
                        "ffmpeg",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc2=size=160x90:rate=5",
                        "-t",
                        "1",
                        "-c:v",
                        "libx264",
                        *output_args("x" * 43),
                    ],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=20,
                    check=False,
                )
                assert rejected.returncode != 0, "unknown RTMP key must not publish"
            else:
                rejected = requests.post(
                    https_ingest,
                    headers={"Authorization": f"Bearer {'x' * 43}"},
                    data=b"FLV",
                    verify=False,
                    timeout=20,
                )
                assert rejected.status_code == 401, "unknown HTTPS key must not publish"
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
                    "testsrc2=size=640x360:rate=30",
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
                    "30",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "128k",
                    *output_args(key),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                stdin=subprocess.PIPE,
            )

            def local_live():
                item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                    "media_attachments"
                ][0]
                return item if item["live"]["state"] == "live" else None

            live = wait_for(
                local_live, desc=f"{transport} publisher switches native live on air"
            )
            assert "scheduled_at" not in live["live"], (
                "the announcement is consumed at start"
            )
            assert "p2p" not in live.get("hls", {}), (
                "low-latency playback stays server-only"
            )
            assert live["url"].endswith("/stream.mp4"), live
            assert live["hls"]["master"].endswith("/master.m3u8"), live
            master = wait_for(
                lambda: _ok_text(live["hls"]["master"]),
                desc="native HLS master becomes available",
            )
            child = next(
                line for line in master.splitlines() if line.startswith("/media/")
            )
            playlist = wait_for(
                lambda: _ok_text(config.PLAMENU_URL + child),
                desc="native HLS media playlist becomes available",
            )
            assert "#EXT-X-MEDIA-SEQUENCE:" in playlist
            playlist = wait_for(
                lambda: (
                    text
                    if (text := _ok_text(config.PLAMENU_URL + child))
                    and len(
                        [line for line in text.splitlines() if line.endswith(".ts")]
                    )
                    >= 8
                    else None
                ),
                desc="DVR playlist grows past the old six-segment window",
                timeout=60,
            )
            durations = [
                float(line.removeprefix("#EXTINF:").split(",")[0])
                for line in playlist.splitlines()
                if line.startswith("#EXTINF:")
            ]
            assert max(durations) <= 2.5, durations
            segment = next(
                line for line in playlist.splitlines() if line.endswith(".ts")
            )
            data = requests.get(config.PLAMENU_URL + segment, verify=False, timeout=20)
            data.raise_for_status()
            assert data.content[0] == 0x47, "MPEG-TS sync byte"
            hashes = requests.get(
                f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json",
                verify=False,
                timeout=20,
            )
            hashes.raise_for_status()
            assert hashes.headers["Cache-Control"] == "no-store"
            assert (
                hashes.json()[segment.rsplit("/", 1)[1]]
                == hashlib.sha256(data.content).hexdigest()
            )
            with requests.get(
                live["url"], verify=False, timeout=(5, 30), stream=True
            ) as mp4:
                mp4.raise_for_status()
                prefix = next(mp4.iter_content(chunk_size=4096))
                assert b"ftyp" in prefix[:32], "no-JS MP4 gateway has an init section"
            page = browser.get(
                f"{config.PLAMENU_URL}/web/statuses/{status_id}", timeout=30
            )
            assert "data-hls=" in page.text and "<video" in page.text
            assert "/stream.mp4" in page.text, "no-JS video source"
            assert "Go live" in page.text and 'data-live-dvr="300"' in page.text
            low_latency_ap = plamenu_api.ap_get(
                waiting_status["uri"].removeprefix(config.PLAMENU_URL)
            )
            assert not any(
                link.get("name") == "tracker-websocket"
                for link in low_latency_ap["url"]
            )

        with step("PeerTube receives the on-air Update"):
            remote = wait_for(
                lambda: _remote_video_state(pt, uuid, 1),
                desc="PeerTube sees the live state",
            )
            assert remote["name"] == title
            assert (
                remote["category"]["id"] == 15
                and remote["licence"]["id"] == 2
                and remote["nsfw"]
            )
            assert remote["streamingPlaylists"], "PeerTube can expose the stream"
            remote_live = pt.get(f"/api/v1/videos/live/{uuid}")
            assert remote_live["dvrWindow"] == 300, remote_live
            assert remote_live["latencyMode"] == 3, remote_live
            assert remote_live["schedules"] == [], "start clears the spent announcement"
            remote_playlist = remote["streamingPlaylists"][0]["playlistUrl"]
            assert "#EXTM3U" in _ok_text(remote_playlist)

        with step(
            f"stopping {transport} converts the one-shot live into a replay Video"
        ):
            # Ask the encoder to close its output cleanly. Killing its process
            # is an interrupted connection, not an intentional live stop.
            _, encoder_error = publisher.communicate(input=b"q\n", timeout=15)
            assert publisher.returncode == 0, encoder_error.decode(errors="replace")[
                -700:
            ]
            publisher = None

            def local_replay():
                item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                    "media_attachments"
                ][0]
                return (
                    item
                    if "live" not in item
                    and item["url"]
                    and item["url"].endswith(".mp4")
                    else None
                )

            replay = wait_for(
                local_replay, desc="Plamenu finalizes the replay Video", timeout=180
            )
            assert replay["type"] == "video"
            assert replay["preview_url"].endswith(".jpg")
            assert replay["meta"]["original"]["duration"] > 0
            recorded = requests.get(replay["url"], verify=False, timeout=60)
            recorded.raise_for_status()
            assert b"ftyp" in recorded.content[:32]
            assert len(recorded.content) > 100_000
            assert (
                requests.get(
                    replay["url"],
                    headers={"Range": "bytes=0-31"},
                    verify=False,
                    timeout=20,
                ).status_code
                == 206
            )
            page = browser.get(
                f"{config.PLAMENU_URL}/web/statuses/{status_id}", timeout=30
            )
            assert "<video" in page.text and replay["url"] in page.text
            assert (
                db.conn.execute(
                    "SELECT count(*) FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
                    (int(media_id),),
                ).fetchone()[0]
                == 0
            )
            assert db.conn.execute(
                "SELECT replay_media_id FROM native_live_sessions WHERE media_id = %s ORDER BY generation DESC LIMIT 1",
                (int(media_id),),
            ).fetchone()[0] == int(media_id)
            assert (
                requests.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8",
                    verify=False,
                    timeout=20,
                ).status_code
                == 404
            )
            assert (
                requests.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json",
                    verify=False,
                    timeout=20,
                ).status_code
                == 404
            )
            remote_observation = {}

            def remote_replay_check():
                try:
                    video = pt.video(uuid)
                except peertube.PeertubeError:
                    return None
                remote_observation.update(
                    {
                        "state": video["state"]["id"],
                        "isLive": video["isLive"],
                        "files": len(video["files"]),
                        "streamingPlaylists": len(video["streamingPlaylists"]),
                    }
                )
                return (
                    video
                    if video["state"]["id"] == 1
                    and not video["isLive"]
                    and video["files"]
                    else None
                )

            try:
                remote_replay = wait_for(
                    remote_replay_check,
                    desc="PeerTube imports the replay as ordinary video",
                    timeout=90,
                )
            except TimeoutError as error:
                raise AssertionError(
                    f"{error}; last PeerTube video: {remote_observation}"
                ) from error
            assert remote_replay["name"] == title
            assert (
                remote_replay["category"]["id"] == 15
                and remote_replay["licence"]["id"] == 2
                and remote_replay["nsfw"]
            )
            remote_file = remote_replay["files"][0]["fileUrl"]
            ranged = requests.get(
                remote_file, headers={"Range": "bytes=0-63"}, verify=False, timeout=30
            )
            assert ranged.status_code == 206 and b"ftyp" in ranged.content[:32]
            with tempfile.TemporaryDirectory() as temp:
                received = Path(temp) / "peer-replay.mp4"
                response = requests.get(remote_file, verify=False, timeout=60)
                response.raise_for_status()
                received.write_bytes(response.content)
                subprocess.run(
                    [
                        "ffmpeg",
                        "-v",
                        "error",
                        "-i",
                        str(received),
                        "-t",
                        "1",
                        "-f",
                        "null",
                        "-",
                    ],
                    check=True,
                    timeout=30,
                )
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
            except (ApiError, requests.RequestException):
                log("Disposable cleanup request failed")
        try:
            pt.delete(f"/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}")
        except (peertube.PeertubeError, requests.RequestException):
            log("Disposable cleanup request failed")


def _ok_text(url: str):
    response = requests.get(url, verify=False, timeout=20)
    return response.text if response.status_code == 200 else None


def _remote_video_state(pt, uuid: str, state: int):
    try:
        video = pt.video(uuid)
    except peertube.PeertubeError:
        return None
    return video if video["state"]["id"] == state else None


def _remote_live_schedule(pt, uuid: str, planned: str):
    try:
        live = pt.get(f"/api/v1/videos/live/{uuid}")
    except peertube.PeertubeError:
        return None
    return (
        live
        if live.get("schedules") and live["schedules"][0]["startAt"].startswith(planned)
        else None
    )


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    reverse_of="test_peertube_live_swarm_in_both_first_party_players",
)
def test_native_live_peer_delivery_in_both_first_party_players(
    plamenu_api: Api,
    plamenu_user,
    db: Db,
):
    """An actual RTMP broadcast exchanges SHA-checked segments with PeerTube."""
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("livepeers")[:30]
    title = f"Live peers {name}"
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Live peer tests"
    )
    status_id = None
    publisher = None
    media_id = None
    try:
        pt.post("/api/v1/users/me/subscriptions", uri=f"{name}@{config.PLAMENU_DOMAIN}")
        login = browser.post(
            f"{config.PLAMENU_URL}/login",
            data={"email": plamenu_user.email, "password": plamenu_user.password},
            allow_redirects=False,
            timeout=30,
        )
        assert login.status_code == 303
        form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
        form.raise_for_status()
        csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
        created = browser.post(
            f"{config.PLAMENU_URL}/web/videos/live",
            data={
                "csrf": csrf,
                "channel": channel["id"],
                "title": title,
                "description": "Peer-assisted live test pattern with audible tone.",
                "language": "en",
                "visibility": "public",
                "dvr_minutes": "5",
                "latency": "balanced",
            },
            timeout=30,
        )
        assert created.status_code == 200, created.text[:700]
        status_id = re.search(r"/videos/live/(\d+)", created.text).group(1)
        inputs = re.findall(
            r'<input type="text" readonly value="([^"]+)"', created.text
        )
        assert len(inputs) in (3, 4), created.text[:1000]
        server = inputs[0]
        key = inputs[2] if len(inputs) == 4 else inputs[1]
        waiting_status = plamenu_api.get(f"/api/v1/statuses/{status_id}")
        media_id = waiting_status["media_attachments"][0]["id"]
        uuid = db.conn.execute(
            "SELECT uuid FROM video_publications WHERE status_id = %s",
            (int(status_id),),
        ).fetchone()[0]
        assert "p2p" not in waiting_status["media_attachments"][0].get("hls", {})

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
                "testsrc2=size=640x360:rate=30",
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
                "60",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-f",
                "flv",
                f"{server}/{key}",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            stdin=subprocess.PIPE,
        )

        def live_item():
            item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                "media_attachments"
            ][0]
            return (
                item
                if item.get("live", {}).get("state") == "live"
                and item.get("hls", {}).get("p2p")
                else None
            )

        live = wait_for(
            live_item, desc="on-air broadcast exposes a peer swarm", timeout=60
        )
        swarm = live["hls"]["p2p"]
        assert swarm["trackers"] == [f"wss://{config.PLAMENU_DOMAIN}/tracker/socket"]
        assert swarm["sha256_url"].endswith(f"/media/hls/{media_id}/sha256.json")
        assert swarm["video_uuid"] == uuid
        assert swarm["swarm_id"] == live["hls"]["master"]
        infohash = db.conn.execute(
            "SELECT p2p_infohash FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
            (int(media_id),),
        ).fetchone()[0]
        assert len(infohash) == 20
        ap = plamenu_api.ap_get(waiting_status["uri"].removeprefix(config.PLAMENU_URL))
        links = ap["url"]
        assert any(link.get("name") == "tracker-websocket" for link in links)
        hls_link = next(
            link for link in links if link.get("mediaType") == "application/x-mpegURL"
        )
        assert any(tag.get("name") == infohash for tag in hls_link["tag"])
        assert any(tag.get("name") == "sha256" for tag in hls_link["tag"])

        remote = wait_for(
            lambda: (
                item
                if (item := _remote_video_state(pt, uuid, 1))
                and item["streamingPlaylists"]
                and f"wss://{config.PLAMENU_DOMAIN}/tracker/socket"
                in item["trackerUrls"]
                else None
            ),
            desc="PeerTube imports the peer-enabled live Video",
            timeout=90,
        )
        assert remote["streamingPlaylists"]
        assert f"wss://{config.PLAMENU_DOMAIN}/tracker/socket" in remote["trackerUrls"]
        assert (
            remote["streamingPlaylists"][0]["segmentsSha256Url"] == swarm["sha256_url"]
        )
        wait_for(
            lambda: bool(
                requests.get(swarm["sha256_url"], verify=False, timeout=20).json()
            ),
            desc="the rolling integrity manifest has a committed segment",
            timeout=30,
        )
        with step("Plamenu and PeerTube first-party browsers exchange live segments"):
            subprocess.run(
                [
                    "node",
                    "verify-native-live-p2p.mjs",
                    f"{config.PLAMENU_URL}/web/statuses/{status_id}",
                    f"{config.PEERTUBE_URL}/w/{uuid}",
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                check=True,
                timeout=180,
                env={
                    **os.environ,
                    "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                    "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
                },
            )

        _, encoder_error = publisher.communicate(input=b"q\n", timeout=20)
        assert publisher.returncode == 0, encoder_error.decode(errors="replace")[-700:]
        publisher = None
        wait_for(
            lambda: (
                "live"
                not in plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                    "media_attachments"
                ][0]
            ),
            desc="the completed live is a replay",
            timeout=180,
        )
        assert (
            requests.get(swarm["sha256_url"], verify=False, timeout=20).status_code
            == 404
        )
        assert (
            db.conn.execute(
                "SELECT count(*) FROM native_live_sessions WHERE p2p_infohash = %s AND ended_at IS NULL",
                (infohash,),
            ).fetchone()[0]
            == 0
        )
        assert (
            asyncio.run(_tracker_announce(infohash))["failure reason"]
            == "unknown info_hash"
        )
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
            except (ApiError, requests.RequestException):
                log("Disposable cleanup request failed")
        try:
            pt.delete(f"/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}")
        except (peertube.PeertubeError, requests.RequestException):
            log("Disposable cleanup request failed")


async def _tracker_announce(infohash: str):
    async with websockets.connect(
        f"wss://{config.PLAMENU_DOMAIN}/tracker/socket",
        ssl=ssl._create_unverified_context(),
    ) as socket:
        await socket.send(
            json.dumps(
                {
                    "action": "announce",
                    "info_hash": infohash,
                    "peer_id": "-PM0001-012345678901",
                    "event": "started",
                    "left": 1,
                }
            )
        )
        return json.loads(await asyncio.wait_for(socket.recv(), timeout=10))


@pytest.mark.parametrize(
    "transport",
    [
        "rtmp",
        "https",
        pytest.param(
            "rtmps",
            marks=pytest.mark.capability("PLAMENU_RTMPS"),
        ),
    ],
)
def test_rotating_stream_key_revokes_active_encoder(
    plamenu_api: Api,
    plamenu_user,
    db: Db,
    transport: str,
):
    """A connected key publisher stops; a recurring Video restarts with its new key."""
    browser = requests.Session()
    browser.verify = False
    name = unique("revokelive")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Revocation tests"
    )
    status_id = None
    publishers = []
    progressive = reader = None
    try:
        login = browser.post(
            f"{config.PLAMENU_URL}/login",
            data={"email": plamenu_user.email, "password": plamenu_user.password},
            allow_redirects=False,
            timeout=30,
        )
        assert login.status_code == 303
        form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
        form.raise_for_status()
        csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
        created = browser.post(
            f"{config.PLAMENU_URL}/web/videos/live",
            data={
                "csrf": csrf,
                "channel": channel["id"],
                "title": f"Revocable {name}",
                "description": "Rotating a live stream key",
                "language": "en",
                "visibility": "public",
                "permanent": "yes",
                "dvr_minutes": "5",
                "latency": "balanced",
            },
            timeout=30,
        )
        assert created.status_code == 200, created.text[:700]
        status_id = re.search(r"/videos/live/(\d+)", created.text).group(1)
        inputs = re.findall(
            r'<input type="text" readonly value="([^"]+)"', created.text
        )
        assert len(inputs) in (3, 4)
        server = inputs[0]
        secure_server = inputs[1] if len(inputs) == 4 else None
        old_key = inputs[2] if len(inputs) == 4 else inputs[1]
        https_ingest = inputs[-1]
        media_id = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
            "media_attachments"
        ][0]["id"]
        manage_url = f"{config.PLAMENU_URL}/videos/live/{status_id}"
        manage = browser.get(manage_url, timeout=30)
        assert manage.status_code == 200
        assert 'aria-describedby="live-rotate-warning"' in manage.text

        def output_args(key: str):
            if transport == "rtmp":
                return ["-f", "flv", f"{server}/{key}"]
            if transport == "rtmps":
                return ["-f", "flv", *rtmps_tls_args(), f"{secure_server}/{key}"]
            return [
                "-f",
                "flv",
                "-method",
                "POST",
                "-headers",
                f"Authorization: Bearer {key}\r\n",
                "-tls_verify",
                "0",
                https_ingest,
            ]

        def start(key: str):
            process = subprocess.Popen(
                [
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x180:rate=15",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-g",
                    "30",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "64k",
                    *output_args(key),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                stdin=subprocess.PIPE,
            )
            publishers.append(process)
            return process

        def state(expected: str):
            if expected == "live" and publishers[-1].poll() is not None:
                error = publishers[-1].stderr.read().decode(errors="replace")
                raise AssertionError(
                    f"{transport} publisher exited before {expected}: {error[-1500:]}"
                )
            item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                "media_attachments"
            ][0]
            return item if item.get("live", {}).get("state") == expected else None

        first = start(old_key)
        wait_for(
            lambda: state("live"), desc=f"{transport} first session is live", timeout=40
        )

        def committed_segment():
            response = requests.get(
                f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json",
                verify=False,
                timeout=20,
            )
            return response.status_code == 200 and any(
                name.endswith(".ts") for name in response.json()
            )

        wait_for(
            committed_segment,
            desc="the first live session commits media",
            timeout=30,
        )
        progressive = requests.get(
            f"{config.PLAMENU_URL}/media/live/{media_id}/stream.mp4",
            verify=False,
            stream=True,
            timeout=(30, 30),
        )
        assert progressive.status_code == 200
        chunks = progressive.iter_content(1024)
        assert b"ftyp" in next(chunks), (
            "an already-connected no-JS MP4 viewer must receive initialization"
        )
        finished = threading.Event()
        reader_errors = []

        def drain_live():
            try:
                for _ in chunks:
                    pass
            except requests.RequestException as error:
                reader_errors.append(error)
            finally:
                finished.set()

        reader = threading.Thread(target=drain_live, daemon=True)
        reader.start()
        revoked_at = time.monotonic()
        rotated = browser.post(
            f"{config.PLAMENU_URL}/web/videos/live/{status_id}/rotate-key",
            data={"csrf": csrf},
            timeout=30,
        )
        assert rotated.status_code == 200, rotated.text[:700]
        assert finished.wait(10), (
            "revocation must close a connected progressive viewer within the watchdog bound"
        )
        assert not reader_errors, reader_errors
        assert time.monotonic() - revoked_at < 10
        new_inputs = re.findall(
            r'<input type="text" readonly value="([^"]+)"', rotated.text
        )
        new_key = new_inputs[2] if len(new_inputs) == 4 else new_inputs[1]
        assert len(new_key) == 43 and new_key != old_key
        wait_for(
            lambda: state("waiting"), desc="rotation stops active publisher", timeout=20
        )
        wait_for(
            lambda: first.poll() is not None,
            desc="revoked encoder connection closes",
            timeout=20,
        )
        assert (
            db.conn.execute(
                "SELECT count(*) FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
                (int(media_id),),
            ).fetchone()[0]
            == 0
        )
        assert (
            requests.get(
                f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8",
                verify=False,
                timeout=20,
            ).status_code
            == 404
        )

        if transport == "https":
            rejected = requests.post(
                https_ingest,
                headers={"Authorization": f"Bearer {old_key}"},
                data=b"FLV",
                verify=False,
                timeout=20,
            )
            assert rejected.status_code == 401, rejected.text[:500]
        else:
            rejected = subprocess.run(
                [
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x90:rate=5",
                    "-t",
                    "1",
                    "-c:v",
                    "libx264",
                    *output_args(old_key),
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                timeout=20,
                check=False,
            )
            assert rejected.returncode != 0, "old key must not claim a new session"
        second = start(new_key)
        wait_for(
            lambda: state("live"), desc="new key starts a recurring session", timeout=40
        )
        wait_for(
            committed_segment, desc="the restarted session commits media", timeout=30
        )
        assert (
            db.conn.execute(
                "SELECT max(generation) FROM native_live_sessions WHERE media_id = %s",
                (int(media_id),),
            ).fetchone()[0]
            == 2
        )
        if transport == "https":
            # FFmpeg may tear down a streaming HTTP POST without a final body
            # frame even on `q`; explicit revocation is a clean owner stop.
            stopped = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live/{status_id}/rotate-key",
                data={"csrf": csrf},
                timeout=30,
            )
            assert stopped.status_code == 200
            wait_for(
                lambda: second.poll() is not None,
                desc="second HTTPS publisher is revoked",
                timeout=20,
            )
        else:
            _, encoder_error = second.communicate(input=b"q\n", timeout=20)
            assert second.returncode == 0, encoder_error.decode(errors="replace")[-700:]
        wait_for(lambda: state("waiting"), desc="new session stops", timeout=30)
        wait_for(
            lambda: (
                db.conn.execute(
                    "SELECT count(*) FROM native_live_sessions WHERE media_id = %s AND replay_media_id IS NOT NULL",
                    (int(media_id),),
                ).fetchone()[0]
                == 2
            ),
            desc="both independent session replays are saved",
            timeout=180,
        )
    finally:
        if progressive is not None:
            progressive.close()
        if reader is not None:
            reader.join(timeout=2)
        for publisher in publishers:
            if publisher.poll() is None:
                publisher.terminate()
                try:
                    publisher.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    publisher.kill()
                    publisher.communicate(timeout=10)
        if status_id is not None:
            try:
                plamenu_api.delete(f"/api/v1/statuses/{status_id}")
            except (ApiError, requests.RequestException):
                log("Disposable cleanup request failed")


@pytest.mark.requires_peer("peertube")
@pytest.mark.parametrize(
    "transport",
    [
        "rtmp",
        "https",
        pytest.param(
            "rtmps",
            marks=pytest.mark.capability("PLAMENU_RTMPS"),
        ),
    ],
)
@pytest.mark.parametrize("permanent", [False, True], ids=["one-shot", "recurring"])
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="Outbound native reconnect/replay refinement; the paired core live lifecycle covers the reverse direction, while this case asserts native encoder recovery.",
)
def test_native_live_interruption_and_reconnect(
    plamenu_api: Api, plamenu_user, db: Db, permanent: bool, transport: str
):
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("reconnectlive")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Reconnect tests"
    )
    status_id = None
    publisher = None
    replays = []
    try:
        with step("publish a stable waiting broadcast followed from PeerTube"):
            pt.post(
                "/api/v1/users/me/subscriptions",
                uri=f"{name}@{config.PLAMENU_DOMAIN}",
            )
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303
            form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
            csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
            created = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live",
                data={
                    "csrf": csrf,
                    "channel": channel["id"],
                    "title": f"Reconnect live {name}",
                    "description": "A reconnecting live test pattern.",
                    "visibility": "public",
                    "category": "13",
                    "licence": "9",
                    "spoiler_text": "Flashing recurring test imagery",
                    **({"permanent": "yes"} if permanent else {}),
                },
                timeout=30,
            )
            assert created.status_code == 200, created.text[:700]
            status_id = re.search(r"/videos/live/(\d+)", created.text).group(1)
            inputs = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            if len(inputs) == 4:
                server, secure_server, key, https_ingest = inputs
            else:
                server, key, https_ingest = inputs
                secure_server = None
            media_id = int(
                plamenu_api.get(f"/api/v1/statuses/{status_id}")["media_attachments"][
                    0
                ]["id"]
            )
            uuid = db.conn.execute(
                "SELECT uuid FROM video_publications WHERE status_id = %s",
                (int(status_id),),
            ).fetchone()[0]

        def start_encoder():
            if transport == "rtmp":
                output = ["-f", "flv", f"{server}/{key}"]
            elif transport == "rtmps":
                output = ["-f", "flv", *rtmps_tls_args(), f"{secure_server}/{key}"]
            else:
                output = [
                    "-f",
                    "flv",
                    "-method",
                    "POST",
                    "-headers",
                    f"Authorization: Bearer {key}\r\n",
                    "-tls_verify",
                    "0",
                    https_ingest,
                ]
            return subprocess.Popen(
                [
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x180:rate=15",
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
                    "30",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    *output,
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )

        def media_state(expected):
            item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                "media_attachments"
            ][0]
            return item if item["live"]["state"] == expected else None

        with step("the first session reaches local and PeerTube playback"):
            publisher = start_encoder()
            first = wait_for(lambda: media_state("live"), desc="first session on air")
            first_master = wait_for(
                lambda: _ok_text(first["hls"]["master"]),
                desc="first HLS master",
            )
            first_child = next(
                line for line in first_master.splitlines() if line.startswith("/media/")
            )
            wait_for(
                lambda: _ok_text(config.PLAMENU_URL + first_child),
                desc="first HLS playlist",
            )
            wait_for(
                lambda: _remote_video_state(pt, uuid, 1),
                desc="PeerTube first on-air state",
            )

        with step(
            "an abrupt encoder loss pauses playback and opens a reconnect window"
        ):
            publisher.kill()
            publisher.communicate(timeout=15)
            publisher = None
            interrupted = wait_for(
                lambda: media_state("interrupted"), desc="interrupted state"
            )
            assert interrupted["url"] is None and interrupted.get("hls") is None
            assert (
                requests.get(
                    f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8",
                    verify=False,
                    timeout=20,
                ).status_code
                == 404
            )
            assert db.conn.execute(
                "SELECT reconnect_deadline > now() FROM native_live_broadcasts WHERE media_id = %s",
                (media_id,),
            ).fetchone()[0]
            manage = browser.get(
                f"{config.PLAMENU_URL}/videos/live/{status_id}", timeout=30
            )
            assert "Connection interrupted" in manage.text
            assert "Reconnect your encoder within one minute" in manage.text
            post = browser.get(
                f"{config.PLAMENU_URL}/web/statuses/{status_id}", timeout=30
            )
            assert "Stream interrupted" in post.text
            wait_for(
                lambda: _remote_video_state(pt, uuid, 4), desc="PeerTube off-air update"
            )

        with step("the same key resumes under a new session without a new Video"):
            publisher = start_encoder()
            live = wait_for(
                lambda: media_state("live"), desc="reconnected session on air"
            )
            second_master = wait_for(
                lambda: _ok_text(live["hls"]["master"]),
                desc="reconnected HLS master",
            )
            second_child = next(
                line
                for line in second_master.splitlines()
                if line.startswith("/media/")
            )
            assert second_child != first_child
            assert (
                requests.get(
                    config.PLAMENU_URL + first_child, verify=False, timeout=20
                ).status_code
                == 404
            )
            wait_for(
                lambda: _ok_text(config.PLAMENU_URL + second_child),
                desc="second HLS playlist",
            )
            sessions = db.conn.execute(
                "SELECT generation, ended_at IS NOT NULL FROM native_live_sessions "
                "WHERE media_id = %s ORDER BY generation",
                (media_id,),
            ).fetchall()
            assert sessions == [(1, True), (2, False)]
            wait_for(
                lambda: _remote_video_state(pt, uuid, 1),
                desc="PeerTube reconnected on-air state",
            )

        if permanent:
            with step(
                "a deliberate stop preserves the live Video and publishes both session replays"
            ):
                publisher.terminate()
                publisher.communicate(timeout=15)
                publisher = None
                wait_for(
                    lambda: media_state("waiting"), desc="recurring broadcast waiting"
                )

                def published_sessions():
                    rows = db.conn.execute(
                        "SELECT ls.generation, ls.replay_media_id, m.status_id, v.uuid "
                        "FROM native_live_sessions ls "
                        "LEFT JOIN media_attachments m ON m.id = ls.replay_media_id "
                        "LEFT JOIN video_publications v ON v.status_id = m.status_id "
                        "WHERE ls.media_id = %s ORDER BY ls.generation",
                        (media_id,),
                    ).fetchall()
                    return (
                        rows
                        if len(rows) == 2 and all(row[2] and row[3] for row in rows)
                        else None
                    )

                replays = wait_for(
                    published_sessions,
                    desc="two distinct recurring replay Videos",
                    timeout=240,
                )
                assert [row[0] for row in replays] == [1, 2]
                assert len({row[1] for row in replays}) == 2
                assert len({row[2] for row in replays}) == 2
                assert all(row[1] != media_id for row in replays)
                for (
                    generation,
                    replay_media_id,
                    replay_status_id,
                    replay_uuid,
                ) in replays:
                    local = plamenu_api.get(f"/api/v1/statuses/{replay_status_id}")
                    assert (
                        local["sensitive"]
                        and local["spoiler_text"] == "Flashing recurring test imagery"
                    )
                    attachment = local["media_attachments"][0]
                    assert attachment["video_metadata"]["category"]["id"] == 13
                    assert attachment["video_metadata"]["licence"]["id"] == 9
                    assert int(attachment["id"]) == replay_media_id
                    assert attachment["type"] == "video" and attachment["url"].endswith(
                        ".mp4"
                    )
                    assert attachment["meta"]["original"]["duration"] > 0
                    assert (
                        requests.get(
                            attachment["url"],
                            headers={"Range": "bytes=0-31"},
                            verify=False,
                            timeout=20,
                        ).status_code
                        == 206
                    )
                    local_page = browser.get(
                        f"{config.PLAMENU_URL}/web/statuses/{replay_status_id}",
                        timeout=30,
                    )
                    assert (
                        "<video" in local_page.text
                        and attachment["url"] in local_page.text
                    )

                    def remote_replay(replay_uuid=replay_uuid):
                        try:
                            video = pt.video(replay_uuid)
                        except peertube.PeertubeError:
                            return None
                        return (
                            video
                            if video["state"]["id"] == 1
                            and not video["isLive"]
                            and video["files"]
                            else None
                        )

                    imported = wait_for(
                        remote_replay,
                        desc=f"PeerTube imports replay {generation}",
                        timeout=120,
                    )
                    assert f"Replay {generation}" in imported["name"]
                    assert (
                        imported["category"]["id"] == 13
                        and imported["licence"]["id"] == 9
                        and imported["nsfw"]
                    )
                manage = browser.get(
                    f"{config.PLAMENU_URL}/videos/live/{status_id}", timeout=30
                )
                assert "Session replays" in manage.text
                public_live = requests.get(
                    f"{config.PLAMENU_URL}/web/statuses/{status_id}",
                    verify=False,
                    timeout=30,
                )
                public_live.raise_for_status()
                assert 'aria-label="Session replays"' in public_live.text
                for _, _, replay_status_id, _ in replays:
                    assert f"/web/statuses/{replay_status_id}" in manage.text
                    assert f"/web/statuses/{replay_status_id}" in public_live.text
                retained_live = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                    "media_attachments"
                ][0]["live"]
                assert retained_live == {
                    "state": "waiting",
                    "permanent": True,
                    "dvr_window_seconds": 600,
                    "latency_mode": 1,
                    "calendar_url": f"{config.PLAMENU_URL}/videos/live/{status_id}/calendar.ics",
                    "channel_calendar_url": f"{config.PLAMENU_URL}/videos/channels/{channel['id']}/calendar.ics",
                }
        else:
            with step("a second loss expires and preserves a one-shot replay"):
                publisher.kill()
                publisher.communicate(timeout=15)
                publisher = None
                wait_for(lambda: media_state("interrupted"), desc="second interruption")

                def local_replay():
                    item = plamenu_api.get(f"/api/v1/statuses/{status_id}")[
                        "media_attachments"
                    ][0]
                    return (
                        item
                        if "live" not in item
                        and (item.get("url") or "").endswith(".mp4")
                        else None
                    )

                replay = wait_for(
                    local_replay, desc="reconnect expiry finalizes replay", timeout=180
                )
                local = plamenu_api.get(f"/api/v1/statuses/{status_id}")
                assert (
                    local["sensitive"]
                    and local["spoiler_text"] == "Flashing recurring test imagery"
                )
                assert replay["video_metadata"]["category"]["id"] == 13
                assert replay["video_metadata"]["licence"]["id"] == 9
                assert replay["meta"]["original"]["duration"] > 0
                assert (
                    requests.get(
                        replay["url"],
                        headers={"Range": "bytes=0-31"},
                        verify=False,
                        timeout=20,
                    ).status_code
                    == 206
                )
                assert (
                    requests.get(
                        config.PLAMENU_URL + second_child, verify=False, timeout=20
                    ).status_code
                    == 404
                )
                assert (
                    db.conn.execute(
                        "SELECT replay_media_id FROM native_live_sessions "
                        "WHERE media_id = %s ORDER BY generation DESC LIMIT 1",
                        (media_id,),
                    ).fetchone()[0]
                    == media_id
                )
                imported = wait_for(
                    lambda: (
                        video
                        if (video := _remote_video_state(pt, uuid, 1))
                        and not video["isLive"]
                        and video["files"]
                        else None
                    ),
                    desc="PeerTube imports replay after reconnect expiry",
                    timeout=90,
                )
                assert (
                    imported["category"]["id"] == 13
                    and imported["licence"]["id"] == 9
                    and imported["nsfw"]
                )
    finally:
        if publisher is not None:
            if publisher.poll() is None:
                publisher.kill()
            publisher.communicate(timeout=15)
        for _, _, replay_status_id, _ in replays:
            try:
                plamenu_api.delete(f"/api/v1/statuses/{replay_status_id}")
            except (ApiError, requests.RequestException):
                log("Disposable cleanup request failed")
        if status_id is not None:
            try:
                plamenu_api.delete(f"/api/v1/statuses/{status_id}")
            except (ApiError, requests.RequestException):
                log("Disposable cleanup request failed")
        try:
            pt.delete(f"/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}")
        except (peertube.PeertubeError, requests.RequestException):
            log("Disposable cleanup request failed")
