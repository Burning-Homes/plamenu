"""Real, isolated WebRTC tracker admission/fault and PeerTube playback matrix.

No global-limit overrides, synthetic hashes admitted into SQL, or production
database outages. Only the uniquely named fixture database is locked.
"""

import json
import os
import shutil
import socket
import ssl
import subprocess
import tempfile
import time
from contextlib import ExitStack
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import psycopg
import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _digest
from websockets.exceptions import ConnectionClosed
from websockets.sync.client import connect


def _peer_id(number):
    return f"-PM0001-{number:012d}"


def _message(infohash, number, **fields):
    return {
        "action": "announce",
        "info_hash": infohash,
        "peer_id": _peer_id(number),
        "event": "started",
        "left": 1,
        "offers": [],
        **fields,
    }


def _send(tracker, message):
    tracker.send(json.dumps(message))


def _receive(tracker):
    return json.loads(tracker.recv(timeout=8))


def _closed(tracker, timeout=8):
    with pytest.raises(ConnectionClosed):
        tracker.recv(timeout=timeout)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="unmodified PeerTube and native browser exchange verified bytes after tracker faults",
)
def test_native_tracker_bounds_atomic_signaling_and_fault_recovery():
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable instance port is occupied"
        )
    label = unique("trackerlimits").replace("-", "_")
    peer = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-tracker"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    channel = api = admin = None
    subscribed = suspended = False
    metrics = {}

    def numeric_process_state(pid):
        # Only owned process resource counts; never cmdline, environment,
        # open-file paths, socket peers, account IDs or transport payloads.
        result = {"pid": pid}
        try:
            proc = Path(f"/proc/{pid}")
            stat = (proc / "stat").read_text().rsplit(") ", 1)[1].split()
            result.update(
                start_ticks=int(stat[19]),
                cpu_user_ticks=int(stat[11]),
                cpu_system_ticks=int(stat[12]),
            )
            for line in (proc / "limits").read_text().splitlines():
                for label, prefix in (
                    ("open_files", "Max open files"),
                    ("processes", "Max processes"),
                ):
                    if line.startswith(prefix):
                        values = line[len(prefix) :].split()
                        result[label + "_soft_limit"] = (
                            int(values[0]) if values[0].isdigit() else None
                        )
                        result[label + "_hard_limit"] = (
                            int(values[1]) if values[1].isdigit() else None
                        )
            cgroup = next(
                (
                    line.split(":", 2)[2]
                    for line in (proc / "cgroup").read_text().splitlines()
                    if line.startswith("0::")
                ),
                None,
            )
            if cgroup is not None:
                directory = Path("/sys/fs/cgroup") / cgroup.lstrip("/")
                for filename in ("pids.current", "pids.max"):
                    path = directory / filename
                    if path.is_file():
                        value = path.read_text().strip()
                        result["cgroup_" + filename.replace(".", "_")] = (
                            int(value) if value.isdigit() else None
                        )
                path = directory / "pids.events"
                if path.is_file():
                    result["cgroup_pid_limit_hits"] = int(
                        dict(
                            line.split() for line in path.read_text().splitlines()
                        ).get("max", 0)
                    )
            result["fd_count"] = sum(1 for _ in (proc / "fd").iterdir())
            for line in (proc / "status").read_text().splitlines():
                name, _, value = line.partition(":")
                if name in {"Threads", "VmRSS"}:
                    result[name] = int(value.split()[0])
        except (OSError, ValueError):
            result["unavailable"] = True
        return result

    def tracker():
        return connect(
            f"wss://{peer.domain}/tracker/socket",
            ssl=ssl._create_unverified_context(),
            open_timeout=10,
            close_timeout=3,
        )

    def rejected(message, reason):
        with tracker() as connection:
            _send(connection, message)
            assert _receive(connection) == {"failure reason": reason}
            _closed(connection)

    try:
        with step(
            "publish two real ladders and import the first into unmodified PeerTube"
        ):
            peer.start()
            peer.apply_suite_settings()
            metrics["executable_sha256"] = _digest(Path(f"/proc/{peer._proc.pid}/exe"))
            user, api = peer.user_api("trackercreator")
            _, admin = peer.admin("trackeroperator")
            owner = api.get("/api/v1/accounts/verify_credentials")["id"]
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("trackerfilms")[:30],
                display_name="Tracker films",
            )
            handle = f"{channel['username']}@{peer.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: peer.db.follower_count(channel["username"]) > 0,
                desc="PeerTube follows tracker creator",
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
                evidence / "sample.mp4", seconds=30, size="640x480"
            )
            hashes, publications = [], []
            for index in range(2):
                title = f"Tracker film {label} {index}"
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
                            *(
                                (key, (None, str(value)))
                                for key, value in fields.items()
                            ),
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
                    lambda media=media: peer.db_value(
                        "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                        int(media),
                    ),
                    desc="actual native ladder completes",
                    timeout=180,
                )
                status = api.post(
                    "/api/v1/videos",
                    channel_id=channel["id"],
                    media_id=media,
                    title=title,
                    visibility="public",
                )
                publications.append({"url": status["url"], "title": title})
                with peer.db.conn.cursor() as cursor:
                    cursor.execute(
                        "SELECT p2p_infohash FROM native_video_hls_renditions WHERE media_id=%s ORDER BY height",
                        (int(media),),
                    )
                    hashes.extend(row[0] for row in cursor.fetchall())
            assert len(set(hashes)) >= 5, (
                "four-swarm ceiling needs five actually eligible hashes"
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
                        if video["name"] == publications[0]["title"]
                    ),
                    None,
                ),
                desc="actual PeerTube imports tracker film",
            )

        with step(
            "reject whole malformed messages before any acknowledgment or partial relay"
        ):
            with tracker() as first, tracker() as second:
                for connection, number in ((first, 1), (second, 2)):
                    _send(connection, _message(hashes[0], number))
                    assert _receive(connection)["action"] == "announce"
                good = {
                    "offer": {"type": "offer", "sdp": "valid-shaped-test"},
                    "offer_id": _peer_id(90),
                }
                rejected(
                    _message(
                        hashes[0], 3, offers=[good, {**good, "offer_id": "short"}]
                    ),
                    "invalid offer_id",
                )
                for connection in (first, second):
                    with pytest.raises(TimeoutError):
                        connection.recv(timeout=0.5)
            for fields, reason in [
                ({"offers": {}}, "invalid offers"),
                ({"offers": [good] * 17}, "too many offers"),
                (
                    {"offers": [{**good, "offer": {"sdp": "a" * 16384}}]},
                    "offer too large",
                ),
                ({"answer": None}, "invalid answer"),
                ({"answer": {}, "to_peer_id": "short"}, "invalid to_peer_id"),
                ({"answer": {"sdp": "a" * 16384}}, "answer too large"),
                ({"peer_id": "\u0100" * 20}, "invalid peer_id"),
                ({"event": None}, "invalid event"),
            ]:
                rejected(_message(hashes[0], 4, **fields), reason)
            rejected(_message("AAAAAAAAAAAAAAAAAAAA", 4), "unknown info_hash")

        with (
            step("relay valid binary-ID offers/answers only to the same swarm"),
            tracker() as first,
            tracker() as second,
            tracker() as foreign,
        ):
            for connection, infohash, number in (
                (first, hashes[0], 10),
                (foreign, hashes[1], 11),
            ):
                _send(connection, _message(infohash, number))
                _receive(connection)
            binary = "\u00ff" * 20
            _send(second, _message(hashes[0], 12, peer_id=binary, offers=[good]))
            _receive(second)
            offered = _receive(first)
            assert offered["offer"] == good["offer"] and offered["peer_id"] == binary
            _send(
                first,
                _message(
                    hashes[0],
                    10,
                    answer={"type": "answer", "sdp": "valid-shaped-test"},
                    to_peer_id=binary,
                    offer_id=good["offer_id"],
                ),
            )
            answered = _receive(second)
            assert answered["answer"]["type"] == "answer" and answered[
                "peer_id"
            ] == _peer_id(10)
            _send(
                first,
                _message(
                    hashes[0],
                    10,
                    answer={},
                    to_peer_id=_peer_id(11),
                    offer_id=good["offer_id"],
                ),
            )
            with pytest.raises(TimeoutError):
                foreign.recv(timeout=0.5)
            rejected(_message(hashes[0], 10), "peer_id already in use")
            _send(first, _message(hashes[0], 13))
            assert _receive(first) == {
                "failure reason": "one peer_id per socket and swarm"
            }
            _closed(first)

        with step(
            "enforce four swarms, ninety announces and the binary/frame ceilings"
        ):
            with tracker() as connection:
                for infohash in hashes[:4]:
                    _send(connection, _message(infohash, 20))
                    assert _receive(connection)["info_hash"] == infohash
                _send(connection, _message(hashes[4], 20))
                assert _receive(connection) == {"failure reason": "too many swarms"}
                _closed(connection)
            with tracker() as connection:
                for _ in range(90):
                    _send(connection, _message(hashes[0], 21))
                    assert _receive(connection)["action"] == "announce"
                _send(connection, _message(hashes[0], 21))
                _closed(connection)
            for payload in (b"unsupported binary", " " * 65537):
                with tracker() as connection:
                    connection.send(payload)
                    _closed(connection)

        with step(
            "hold 128 admitted peers and reclaim a stopped and disconnected identity"
        ):
            with ExitStack() as connections:
                members = []
                admission_started = time.monotonic()
                for number in range(100, 228):
                    try:
                        connection = connections.enter_context(tracker())
                    except Exception:
                        metrics["failed_admission"] = {
                            "attempt_ordinal": number - 99,
                            "already_admitted": len(members),
                            "elapsed_seconds": time.monotonic() - admission_started,
                            "test_process": numeric_process_state(os.getpid()),
                            "server_process": numeric_process_state(peer._proc.pid),
                        }
                        (evidence / "tracker-admission-failure.json").write_text(
                            json.dumps(metrics["failed_admission"], indent=2)
                        )
                        (evidence / "tracker-admission-failure.json").chmod(0o600)
                        raise
                    _send(connection, _message(hashes[0], number))
                    response = _receive(connection)
                    assert response["incomplete"] == number - 99, response
                    members.append(connection)
                metrics["peak_admission"] = {
                    "admitted": len(members),
                    "elapsed_seconds": time.monotonic() - admission_started,
                    "test_process": numeric_process_state(os.getpid()),
                    "server_process": numeric_process_state(peer._proc.pid),
                }
                rejected(_message(hashes[0], 228), "swarm full")
                _send(members[0], _message(hashes[0], 100, event="stopped"))
                _receive(members[0])
                with tracker() as replacement:
                    _send(replacement, _message(hashes[0], 100))
                    assert _receive(replacement)["incomplete"] == 128
                    replacement.close()
                # The closing handshake finishes before this new announce;
                # no arbitrary sleep/retry may mask leaked registry state.
                with tracker() as replacement:
                    _send(replacement, _message(hashes[0], 100))
                    assert _receive(replacement)["incomplete"] == 128
            metrics["simultaneous_swarm_peers"] = 128

        with step(
            "bound idle first admission and fail closed during an isolated DB lock"
        ):
            before = time.monotonic()
            with tracker() as idle:
                _closed(idle, timeout=20)
            metrics["unannounced_close_seconds"] = time.monotonic() - before
            assert 14 <= metrics["unannounced_close_seconds"] <= 20
            with psycopg.connect(peer.dsn) as blocker:
                blocker.execute(
                    "LOCK TABLE native_video_hls_renditions IN ACCESS EXCLUSIVE MODE"
                )
                before = time.monotonic()
                rejected(_message(hashes[0], 300), "tracker unavailable")
                metrics["locked_authority_refusal_seconds"] = time.monotonic() - before
                assert metrics["locked_authority_refusal_seconds"] <= 5
                blocker.rollback()
            with tracker() as recovered:
                _send(recovered, _message(hashes[0], 300))
                assert _receive(recovered)["action"] == "announce"

        with (
            step("canonical suspension refuses attached-peer reannounces before relay"),
            tracker() as attached,
        ):
            _send(attached, _message(hashes[0], 400))
            _receive(attached)
            admin.post(
                f"/api/v1/admin/accounts/{owner}/action",
                type="suspend",
                text="Owned AP09 tracker fixture",
            )
            suspended = True
            _send(attached, _message(hashes[0], 400, offers=[good]))
            assert _receive(attached) == {"failure reason": "unknown info_hash"}
            _closed(attached)
            rejected(_message(hashes[0], 401), "unknown info_hash")
            admin.post(f"/api/v1/admin/accounts/{owner}/unsuspend")
            suspended = False

        with step(
            "decode actual PeerTube/native peer exchange after all admission and DB faults"
        ):
            result = subprocess.run(
                [
                    "node",
                    "verify-native-p2p.mjs",
                    publications[0]["url"],
                    f"{config.PEERTUBE_URL}/w/{remote['uuid']}",
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                capture_output=True,
                text=True,
                timeout=180,
                env={
                    **os.environ,
                    "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                    "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
                },
                check=False,
            )
            (evidence / "peer-browser.log").write_text(result.stdout + result.stderr)
            assert result.returncode == 0, result.stdout[-3000:] + result.stderr[-3000:]
            metrics["peer_browser"] = json.loads(result.stdout.strip().splitlines()[-1])
        (evidence / "result.json").write_text(json.dumps(metrics, indent=2))
    finally:
        try:
            if suspended and admin:
                admin.post(f"/api/v1/admin/accounts/{owner}/unsuspend")
            if channel and api:
                api.post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
            if subscribed:
                response = pt.http.delete(
                    f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30
                )
                assert response.status_code in (204, 404)
        finally:
            if peer._dir is not None:
                log = Path(peer._dir.name) / "server.log"
                if log.exists():
                    shutil.copyfile(log, evidence / "server.log")
                    (evidence / "server.log").chmod(0o600)
            peer.stop()
            browser.close()
