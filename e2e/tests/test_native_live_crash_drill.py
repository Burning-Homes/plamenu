"""Opt-in destructive drill for the *local disposable* Plamenu server.

Run only with PLAMENU_CRASH_DRILL=1 and an exact PLAMENU_SERVER_PID. The test
SIGKILLs that process, starts the same candidate binary with the same config,
and leaves the replacement serving. Never include this in the default suite.
"""

import os
import re
import signal
import subprocess
import time
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.steps import step, wait_for


pytestmark = pytest.mark.skipif(
    os.environ.get("PLAMENU_CRASH_DRILL") != "1",
    reason="destructive local-server restart drill is opt-in",
)


def _ready():
    try:
        return requests.get(f"{config.PLAMENU_URL}/ready", verify=False, timeout=2).status_code == 200
    except requests.RequestException:
        return False


def _exact_local_server(pid: int):
    root = config.PLAMENU_DIR.resolve()
    cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
    args = [part.decode() for part in cmdline if part]
    assert args[0].endswith("/plamenu") and args[-3:] == [
        "--config", config.PLAMENU_CONFIG, "serve"
    ], args
    assert Path(f"/proc/{pid}/cwd").resolve() == root
    binary = Path(config.PLAMENU_BINARY or "").resolve()
    assert binary == root / "target/debug/plamenu" and binary.is_file()
    invoked = Path(args[0])
    assert (invoked if invoked.is_absolute() else root / invoked).resolve() == binary
    assert Path(f"/proc/{pid}/exe").resolve() == binary
    return binary


def test_native_live_survives_server_crash(plamenu_api, plamenu_user, db):
    pid = int(os.environ["PLAMENU_SERVER_PID"])
    binary = _exact_local_server(pid)
    assert _ready()
    browser = requests.Session()
    browser.verify = False
    name = unique("crashlive")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Crash drill"
    )
    publisher = None
    replacement = None
    status_id = None
    replay_status_ids = []
    killed = False
    try:
        with step("create and start a disposable recurring live Video"):
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False, timeout=30,
            )
            assert login.status_code == 303
            page = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
            csrf = re.search(r'name="csrf" value="([^"]+)"', page.text).group(1)
            created = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live",
                data={
                    "csrf": csrf, "channel": channel["id"],
                    "title": f"Crash drill {name}", "description": "Disposable restart drill.",
                    "visibility": "public", "permanent": "yes",
                }, timeout=30,
            )
            assert created.status_code == 200
            status_id = re.search(r'/videos/live/(\d+)', created.text).group(1)
            fields = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            assert len(fields) in (3, 4)
            server, key = fields[0], fields[-2]

            def start_encoder():
                return subprocess.Popen(
                    [
                        "ffmpeg", "-hide_banner", "-loglevel", "error", "-re",
                        "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=15",
                        "-f", "lavfi", "-i", "sine=frequency=440",
                        "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                        "-g", "30", "-pix_fmt", "yuv420p", "-c:a", "aac",
                        "-f", "flv", f"{server}/{key}",
                    ], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                )

            def state(expected):
                media = plamenu_api.get(f"/api/v1/statuses/{status_id}")["media_attachments"][0]
                return media if media["live"]["state"] == expected else None

            publisher = start_encoder()
            live = wait_for(lambda: state("live"), desc="first live session")
            media_id = int(live["id"])
            first_session = db.conn.execute(
                "SELECT session_id FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
                (media_id,),
            ).fetchone()[0]
            media_dir = Path(config.PLAMENU_CONFIG).read_text()
            configured = re.search(r'^media_dir\s*=\s*"([^"]+)"', media_dir, re.MULTILINE)
            assert configured, "the disposable media directory must be explicit"
            recording = Path(configured.group(1)) / "native-live" / str(media_id) / first_session / "replay.mp4"
            wait_for(lambda: recording.exists() and recording.stat().st_size > 100_000,
                     desc="recurring crash recording has committed a fragment")

        with step("SIGKILL only the verified local server and restart the candidate"):
            assert _exact_local_server(pid) == binary
            os.kill(pid, signal.SIGKILL)
            killed = True
            time.sleep(1)
            replacement = subprocess.Popen(
                [str(binary), "--config", config.PLAMENU_CONFIG, "serve"],
                cwd=config.PLAMENU_DIR,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            wait_for(lambda: _ready() and replacement.poll() is None, desc="replacement ready")
            publisher.communicate(timeout=15)
            publisher = None

        with step("startup recovery announces interruption and revokes old playback"):
            interrupted = wait_for(lambda: state("interrupted"), desc="recovered interrupted state")
            assert interrupted["url"] is None and interrupted.get("hls") is None
            assert db.conn.execute(
                "SELECT ended_at IS NOT NULL FROM native_live_sessions WHERE session_id = %s",
                (first_session,),
            ).fetchone()[0]
            assert requests.get(
                f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8",
                verify=False, timeout=20,
            ).status_code == 404

        with step("the same key and Video reconnect after process replacement"):
            publisher = start_encoder()
            wait_for(lambda: state("live"), desc="post-crash live session")
            generations = db.conn.execute(
                "SELECT generation, ended_at IS NOT NULL FROM native_live_sessions "
                "WHERE media_id = %s ORDER BY generation", (media_id,),
            ).fetchall()
            assert generations == [(1, True), (2, False)]
            publisher.terminate()
            publisher.communicate(timeout=15)
            publisher = None
            wait_for(lambda: state("waiting"), desc="recurring stream waiting after stop")

        with step("both the crashed and cleanly stopped sessions become separate replay Videos"):
            def published_sessions():
                rows = db.conn.execute(
                    "SELECT ls.generation, ls.replay_media_id, m.status_id "
                    "FROM native_live_sessions ls "
                    "LEFT JOIN media_attachments m ON m.id = ls.replay_media_id "
                    "WHERE ls.media_id = %s ORDER BY ls.generation", (media_id,),
                ).fetchall()
                return rows if len(rows) == 2 and all(row[2] for row in rows) else None

            replays = wait_for(published_sessions, desc="recovered recurring replay posts", timeout=240)
            assert [row[0] for row in replays] == [1, 2]
            assert len({row[1] for row in replays}) == 2
            replay_status_ids = [row[2] for row in replays]
            for _, replay_media_id, replay_status_id in replays:
                item = plamenu_api.get(f"/api/v1/statuses/{replay_status_id}")["media_attachments"][0]
                assert int(item["id"]) == replay_media_id
                assert item["meta"]["original"]["duration"] > 0
                assert requests.get(item["url"], headers={"Range": "bytes=0-63"},
                                    verify=False, timeout=20).status_code == 206
            assert int(state("waiting")["id"]) == media_id
            wait_for(lambda: not recording.exists(), desc="crashed recurring scratch cleaned")
    finally:
        if publisher is not None:
            if publisher.poll() is None:
                publisher.kill()
            publisher.communicate(timeout=15)
        if killed and (replacement is None or replacement.poll() is not None):
            subprocess.Popen(
                [str(binary), "--config", config.PLAMENU_CONFIG, "serve"],
                cwd=config.PLAMENU_DIR, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True,
            )
        for replay_status_id in replay_status_ids:
            if _ready():
                try:
                    plamenu_api.delete(f"/api/v1/statuses/{replay_status_id}")
                except Exception:
                    pass
        if status_id is not None and _ready():
            try:
                plamenu_api.delete(f"/api/v1/statuses/{status_id}")
            except Exception:
                pass


def test_one_shot_replay_survives_server_crash(plamenu_api, plamenu_user, db):
    """The unclosed recording, not just the live identity, survives SIGKILL."""
    pid = int(os.environ["PLAMENU_SERVER_PID"])
    binary = _exact_local_server(pid)
    assert _ready()
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("replaycrash")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Replay crash drill"
    )
    publisher = None
    replacement = None
    status_id = None
    killed = False
    subscribed = False
    try:
        with step("create a one-shot Video followed by PeerTube and record fragments"):
            pt.post("/api/v1/users/me/subscriptions", uri=f"{name}@{config.PLAMENU_DOMAIN}")
            subscribed = True
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False, timeout=30,
            )
            assert login.status_code == 303
            form = browser.get(f"{config.PLAMENU_URL}/videos/live/new", timeout=30)
            csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
            created = browser.post(
                f"{config.PLAMENU_URL}/web/videos/live",
                data={"csrf": csrf, "channel": channel["id"],
                      "title": f"Recovered replay {name}",
                      "description": "A recording that survives a server crash.",
                      "visibility": "public"},
                timeout=30,
            )
            assert created.status_code == 200, created.text[:500]
            status_id = re.search(r'/videos/live/(\d+)', created.text).group(1)
            fields = re.findall(r'<input type="text" readonly value="([^"]+)"', created.text)
            server, key = fields[0], fields[-2]
            publisher = subprocess.Popen(
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-re",
                 "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=15",
                 "-f", "lavfi", "-i", "sine=frequency=440",
                 "-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency",
                 "-g", "30", "-pix_fmt", "yuv420p", "-c:a", "aac",
                 "-f", "flv", f"{server}/{key}"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )

            def attachment():
                return plamenu_api.get(f"/api/v1/statuses/{status_id}")["media_attachments"][0]

            live = wait_for(
                lambda: item if (item := attachment())["live"]["state"] == "live" else None,
                desc="one-shot on air",
            )
            media_id = int(live["id"])
            session_id = db.conn.execute(
                "SELECT session_id FROM native_live_sessions WHERE media_id = %s AND ended_at IS NULL",
                (media_id,),
            ).fetchone()[0]
            uuid = db.conn.execute(
                "SELECT uuid FROM video_publications WHERE status_id = %s", (int(status_id),),
            ).fetchone()[0]
            recording = config.PLAMENU_DIR / "media" / "native-live" / str(media_id) / session_id / "replay.mp4"
            # The configured media directory may not be under the source tree.
            media_dir = Path(config.PLAMENU_CONFIG).read_text()
            configured = re.search(r'^media_dir\s*=\s*"([^"]+)"', media_dir, re.MULTILINE)
            if configured:
                recording = Path(configured.group(1)) / "native-live" / str(media_id) / session_id / "replay.mp4"
            wait_for(lambda: recording.exists() and recording.stat().st_size > 100_000,
                     desc="fragmented MP4 has committed video packets")

        with step("kill the exact local server while the encoder is publishing"):
            assert _exact_local_server(pid) == binary
            os.kill(pid, signal.SIGKILL)
            killed = True
            time.sleep(1)
            replacement = subprocess.Popen(
                [str(binary), "--config", config.PLAMENU_CONFIG, "serve"],
                cwd=config.PLAMENU_DIR, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True,
            )
            wait_for(lambda: _ready() and replacement.poll() is None, desc="replacement ready")
            publisher.communicate(timeout=15)
            publisher = None

        with step("restart retains the recording while revoking interrupted live playback"):
            interrupted = wait_for(
                lambda: item if (item := attachment()).get("live", {}).get("state") == "interrupted" else None,
                desc="recovered interruption",
            )
            assert interrupted["url"] is None and interrupted.get("hls") is None
            assert recording.exists() and recording.stat().st_size > 0
            assert db.conn.execute(
                "SELECT ended_at IS NOT NULL FROM native_live_sessions WHERE session_id = %s",
                (session_id,),
            ).fetchone()[0]

        with step("the expired one-shot becomes one playable federated VOD"):
            replay = wait_for(
                lambda: item if "live" not in (item := attachment())
                and (item.get("url") or "").endswith(".mp4") else None,
                desc="crash recording finalized as VOD", timeout=180,
            )
            assert replay["meta"]["original"]["duration"] > 0
            assert requests.get(replay["url"], headers={"Range": "bytes=0-63"},
                                verify=False, timeout=20).status_code == 206
            assert db.conn.execute(
                "SELECT replay_media_id FROM native_live_sessions WHERE session_id = %s",
                (session_id,),
            ).fetchone()[0] == media_id
            def remote_replay():
                try:
                    video = pt.video(uuid)
                except peertube.PeertubeError:
                    return None
                return video if video["state"]["id"] == 1 and not video["isLive"] \
                    and video["files"] else None

            remote = wait_for(remote_replay, desc="PeerTube imports recovered recording", timeout=90)
            assert requests.get(remote["files"][0]["fileUrl"],
                                headers={"Range": "bytes=0-63"},
                                verify=False, timeout=30).status_code == 206
            wait_for(lambda: not recording.exists(), desc="recovered scratch cleaned")
    finally:
        if publisher is not None:
            if publisher.poll() is None:
                publisher.kill()
            publisher.communicate(timeout=15)
        if killed and (replacement is None or replacement.poll() is not None):
            subprocess.Popen(
                [str(binary), "--config", config.PLAMENU_CONFIG, "serve"],
                cwd=config.PLAMENU_DIR, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True,
            )
        if status_id is not None and _ready():
            try:
                plamenu_api.delete(f"/api/v1/statuses/{status_id}")
            except Exception:
                pass
        if subscribed and _ready():
            try:
                pt.delete(f"/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}")
            except Exception:
                pass
