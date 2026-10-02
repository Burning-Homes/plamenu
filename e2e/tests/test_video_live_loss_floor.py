"""Owned replica loss / both-disk floors, with retained-generation recovery."""

import asyncio
import errno
import json
import os
import re
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

import pytest
import requests
import tomllib
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from tests.test_video_backup_restore import _csrf, _digest, _stop_process
from tests.test_video_live_backup_restore import _announce, _stop_encoder
from tests.test_video_live_concurrency import _clock_publisher, _port
from tests.test_video_live_disk_continuity import _muxers, _stage
from tests.test_video_offload import _freeze
from tools.primary_io_fixture import PrimaryFilesystem


@pytest.mark.skipif(
    os.environ.get("AP09_LIVE_DISK_VERIFY") != "1",
    reason="owned actual paired disk loss/floor drill",
)
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="actual PeerTube state and recovered independent replay",
)
@pytest.mark.parametrize("mode", ["replica-eio", "both-full", "both-eio"])
def test_paired_live_loss_floor_and_recovered_generation(mode, monkeypatch):
    assert peertube.reachable()
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0
    label = unique("livefloor").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-live-floor"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-{mode}-", dir=parent))
    evidence.chmod(0o700)
    origin = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    filesystems = []
    publisher = observer = channel = api = pt = None
    observer_log = None
    subscribed = False
    session = requests.Session()
    session.verify = False
    try:
        with step(
            "one frozen native origin and two exclusively owned actual filesystem fixtures"
        ):
            origin.start()
            candidate = _freeze(origin, evidence, monkeypatch)
            _stop_process(origin)
            primary = PrimaryFilesystem(evidence / "primary")
            filesystems.append(primary)
            replica = PrimaryFilesystem(evidence / "replica")
            filesystems.append(replica)
            settings = tomllib.loads(origin._config_path.read_text())
            text = origin._config_path.read_text().replace(
                json.dumps(settings["media_dir"]), json.dumps(str(primary.mount))
            )
            text = text.replace(
                'live_ingest_bind = "127.0.0.1:0"',
                f'live_ingest_bind = "127.0.0.1:{_port()}"',
            )
            text += f"\n[media_mirror]\ndirectory={json.dumps(str(replica.mount))}\nbandwidth_mib_per_second=32\nmin_free_gib=1\n"
            origin._config_path.write_text(text)
            origin._config_path.chmod(0o600)
            origin._launch(120, append_log=True)
            origin.apply_suite_settings()
            user, api = origin.user_api("floorcaster")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("floorlive")[:30],
                display_name="Paired live faults",
            )
            pt = peertube.root()
            handle = f"{channel['username']}@{origin.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: origin.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube follows owned channel",
            )
            assert (
                session.post(
                    origin.url + "/login",
                    data={"email": user.email, "password": user.password},
                    allow_redirects=False,
                    timeout=30,
                ).status_code
                == 303
            )
            title = f"Paired live {mode} {label}"
            created = session.post(
                origin.url + "/web/videos/live",
                data={
                    "csrf": _csrf(session.get(origin.url + "/videos", timeout=30).text),
                    "channel": channel["id"],
                    "title": title,
                    "description": "Moving clock and tone",
                    "visibility": "public",
                    "permanent": "yes",
                    "latency": "balanced",
                    "dvr_minutes": "5",
                },
                timeout=30,
            )
            assert created.status_code == 200
            status = re.search(r"/videos/live/(\d+)", created.text).group(1)
            server, key, _ = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            media = int(api.get_status(status)["media_attachments"][0]["id"])
            publisher = _clock_publisher(server, key)
            wait_for(
                lambda: origin.db_value(
                    "SELECT live_state='live' FROM media_attachments WHERE id=%s", media
                ),
                desc="owned clock on air",
            )
            generation = origin.db_value(
                "SELECT session_id FROM native_live_sessions WHERE media_id=%s AND ended_at IS NULL",
                media,
            )
            relative = Path("native-live") / str(media) / generation
            wait_for(
                lambda: (replica.mount / relative / "index.durable.m3u8").exists(),
                desc="actual paired fsync acknowledgement",
            )
            identities = _muxers(origin)
            assert len(identities) == 1
            remote = wait_for(
                lambda: next(
                    (
                        v
                        for v in pt.get(
                            "/api/v1/users/me/subscriptions/videos",
                            count=100,
                            sort="-publishedAt",
                        )["data"]
                        if v["name"] == title
                    ),
                    None,
                ),
                desc="actual PeerTube receives live Video",
            )
            if mode == "replica-eio":
                manifest = evidence / "viewers.json"
                manifest.write_text(
                    json.dumps(
                        {
                            "local": f"{origin.url}/web/statuses/{status}",
                            "remote": f"{config.PEERTUBE_URL}/w/{remote['uuid']}",
                        }
                    )
                )
                observer_log = (evidence / "browser.log").open("w")
                observer = subprocess.Popen(
                    [
                        "node",
                        str(
                            config.PLAMENU_DIR
                            / "accessibility/verify-live-disk-continuity.mjs"
                        ),
                        str(manifest),
                        str(evidence),
                    ],
                    stdout=observer_log,
                    stderr=subprocess.STDOUT,
                    env={
                        **os.environ,
                        "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                        "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
                    },
                )
                wait_for(
                    lambda: _stage(observer, evidence, "ready-for-fault"),
                    desc="three healthy decoders before replica loss",
                    timeout=90,
                )
            else:
                time.sleep(8)
            selected = filesystems if mode.startswith("both") else [replica]
            fault = "ENOSPC" if mode == "both-full" else "EIO"
            began = time.monotonic()
            for filesystem in selected:
                filesystem.set_fault(fault)
            with pytest.raises(OSError) as actual:
                (selected[-1].mount / "actual-fault-probe").write_bytes(
                    b"must-not-write"
                )
            assert actual.value.errno == getattr(errno, fault)
            if mode == "replica-eio":
                with step(
                    "lost replica leaves same primary writer and real JS/no-JS/PeerTube decoders progressing"
                ):
                    (evidence / "fault-active.json").write_text(
                        json.dumps({"errno": actual.value.errno, "mode": mode})
                    )
                    wait_for(
                        lambda: _stage(observer, evidence, "ready-for-repair"),
                        desc="all decoders sustain actual replica EIO",
                        timeout=45,
                    )
                    assert _muxers(origin) == identities and publisher.poll() is None
                    assert origin.db_value(
                        "SELECT live_state='live' FROM media_attachments WHERE id=%s",
                        media,
                    )
                    for filesystem in selected:
                        filesystem.set_fault(None)
                    (evidence / "fault-repaired.json").write_text("{}")
                    assert observer.wait(timeout=35) == 0
                    _stop_encoder(publisher)
                    publisher = None
            else:
                with step(
                    "both lost/full roots stop on-air authority and preserve the closed generation for repair"
                ):
                    wait_for(
                        lambda: origin.db_value(
                            "SELECT live_state<>'live' FROM media_attachments WHERE id=%s",
                            media,
                        ),
                        desc="no healthy disk stops on-air state",
                        timeout=10,
                    )
                    stopped_seconds = time.monotonic() - began
                    wait_for(
                        lambda: not _muxers(origin),
                        desc="failed native encoder is reaped",
                        timeout=10,
                    )
                    infohash = origin.db_value(
                        "SELECT p2p_infohash FROM native_live_sessions WHERE session_id=%s",
                        generation,
                    )
                    assert (
                        asyncio.run(_announce(origin.domain, infohash))[
                            "failure reason"
                        ]
                        == "unknown info_hash"
                    )
                    assert (
                        requests.get(
                            f"{origin.url}/media/hls/{media}/local/{generation}/index.m3u8",
                            verify=False,
                            timeout=20,
                        ).status_code
                        == 404
                    )
                    _stop_encoder(publisher)
                    publisher = None
                    for filesystem in selected:
                        filesystem.set_fault(None)
                    # Restart only this origin, after real child exit; its
                    # closed-generation recovery must use the preserved prefix.
                    _stop_process(origin)
                    for name, filesystem in [
                        ("primary", primary),
                        ("replica", replica),
                    ]:
                        movie = filesystem.backing / relative / "replay.mp4"
                        assert movie.exists() and movie.stat().st_size > 0
                        shutil.copyfile(
                            movie, evidence / f"{name}-failed-recording.mp4"
                        )
                    origin._launch(120, append_log=True)

            def ready_replay():
                return origin.db_value(
                    "SELECT m.status_id FROM native_live_sessions s JOIN media_attachments m ON m.id=s.replay_media_id WHERE s.session_id=%s AND m.processing='complete'",
                    generation,
                )

            replay_status = wait_for(
                ready_replay,
                desc="repaired closed generation becomes its one independent replay",
                timeout=240,
            )
            replay = api.get_status(replay_status)["media_attachments"][0]
            content = requests.get(replay["url"], verify=False, timeout=30)
            content.raise_for_status()
            decoded = evidence / "recovered-replay.mp4"
            decoded.write_bytes(content.content)
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-xerror",
                    "-i",
                    str(decoded),
                    "-map",
                    "0:v:0",
                    "-map",
                    "0:a:0",
                    "-f",
                    "null",
                    "-",
                ],
                check=True,
                capture_output=True,
                timeout=90,
            )
            peer_uuid = origin.db_value(
                "SELECT uuid::text FROM video_publications WHERE media_id=%s",
                int(replay["id"]),
            )

            def remote_replay():
                try:
                    item = pt.video(peer_uuid)
                    return item if item["files"] else None
                except peertube.PeertubeError:
                    return None

            imported = wait_for(
                remote_replay,
                desc="actual PeerTube accepts independent recovered replay",
                timeout=90,
            )
            ranged = requests.get(
                imported["files"][0]["fileUrl"],
                headers={"Range": "bytes=17-80"},
                verify=False,
                timeout=30,
            )
            assert (
                ranged.status_code == 206 and ranged.content == content.content[17:81]
            )
            (evidence / "result.json").write_text(
                json.dumps(
                    {
                        "candidate_sha256": candidate,
                        "mode": mode,
                        "actual_errno": actual.value.errno,
                        "same_encoder_during_replica_loss": mode == "replica-eio",
                        "stopped_seconds": None
                        if mode == "replica-eio"
                        else stopped_seconds,
                        "recording_preserved": True,
                        "recovered_replay_duration": replay["meta"]["original"][
                            "duration"
                        ],
                        "replay_sha256": _digest(decoded),
                    },
                    indent=2,
                )
            )
            print(f"Protected paired live floor/loss evidence: {evidence}")
    finally:
        for filesystem in filesystems:
            filesystem.set_fault(None)
        if observer is not None and observer.poll() is None:
            observer.terminate()
            observer.wait(timeout=15)
        if observer_log is not None:
            observer_log.close()
        _stop_encoder(publisher)
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
            for filesystem in reversed(filesystems):
                filesystem.stop()
            session.close()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(
                        0o700
                        if path.name in ("candidate-plamenu", "primary-io-fault")
                        else 0o600
                    )
