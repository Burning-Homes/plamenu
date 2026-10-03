"""Whole-store recovery of an interrupted native recording, not same-root restart.

Only a uniquely named disposable plamenu2 instance is killed. Database/media
archives and separately escrowed configuration are retained privately. Paired
roots are logical fixture directories, not a physical two-host disaster claim.
"""

import asyncio
import hashlib
import json
import re
import shutil
import socket
import ssl
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path
from urllib.parse import urlsplit

import psycopg
import pytest
import requests
import tomllib
import websockets
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from psycopg import sql
from tests.test_video_backup_restore import _csrf, _digest, _manifest, _stop_process
from tests.test_video_offload import _freeze


async def _announce(domain, infohash):
    async with websockets.connect(
        f"wss://{domain}/tracker/socket", ssl=ssl._create_unverified_context()
    ) as tracker:
        await tracker.send(
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
        return json.loads(await asyncio.wait_for(tracker.recv(), timeout=10))


def _encoder(server, key, *, seconds=120):
    # Ingest credentials stay in process memory; never echo argv to evidence.
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
            "-t",
            str(seconds),
            "-f",
            "flv",
            f"{server}/{key}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def _stop_encoder(publisher):
    if publisher is not None:
        publisher.terminate()
        try:
            publisher.wait(timeout=15)
        except subprocess.TimeoutExpired:
            publisher.kill()
            publisher.wait(timeout=10)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.parametrize("paired", [False, True], ids=["single-disk", "paired-disks"])
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="a surviving PeerTube subscription imports independently recovered native replay episodes",
)
def test_interrupted_live_recording_restores_into_fresh_database_and_storage(
    paired, monkeypatch
):
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable fixture port is occupied"
        )
    label = unique("liverestore").replace("-", "_")
    source = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    restored = ephemeral.Instance(
        domain=source.domain,
        bind=source.bind,
        db_name=f"plamenu_ap09_{label}_recovered",
    )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-live-restore"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    backup, escrow = evidence / "recovery-set", evidence / "configuration-escrow"
    backup.mkdir(mode=0o700)
    escrow.mkdir(mode=0o700)
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    subscribed = False
    publisher = channel = api = None
    try:
        with step(
            "start a real recurring native encoder with a surviving PeerTube follower"
        ):
            source.start()
            _freeze(source, evidence, monkeypatch)
            _stop_process(source)
            with socket.socket() as allocator:
                allocator.bind(("127.0.0.1", 0))
                rtmp_port = allocator.getsockname()[1]
            original_config = source._config_path.read_text().replace(
                'live_ingest_bind = "127.0.0.1:0"',
                f'live_ingest_bind = "127.0.0.1:{rtmp_port}"',
            )
            if paired:
                replica = Path(source._dir.name) / "replica"
                replica.mkdir()
                original_config += f'\n[media_mirror]\ndirectory = "{replica}"\nbandwidth_mib_per_second = 32\nmin_free_gib = 0\n'
            source._config_path.write_text(original_config)
            source._launch(120, append_log=True)
            source.apply_suite_settings()
            user, api = source.user_api("recoverycaster")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("recoverylive")[:30],
                display_name="Recoverable broadcasts",
            )
            handle = f"{channel['username']}@{source.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: source.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube subscribes before recovery",
            )
            login = browser.post(
                f"{source.url}/login",
                data={"email": user.email, "password": user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303
            title = f"Recovery broadcast {label}"
            created = browser.post(
                f"{source.url}/web/videos/live",
                data={
                    "csrf": _csrf(browser.get(f"{source.url}/videos", timeout=30).text),
                    "channel": channel["id"],
                    "title": title,
                    "description": "Moving pattern and steady tone",
                    "visibility": "public",
                    "permanent": "yes",
                    "latency": "balanced",
                    "dvr_minutes": "5",
                },
                timeout=30,
            )
            assert created.status_code == 200
            status = re.search(r"/videos/live/(\d+)", created.text).group(1)
            fields = re.findall(
                r'<input type="text" readonly value="([^"]+)"', created.text
            )
            assert len(fields) == 3
            server, key, _https = fields
            assert server == f"rtmp://127.0.0.1:{rtmp_port}/live"
            media = int(api.get_status(status)["media_attachments"][0]["id"])
            original_uuid = source.db_value(
                "SELECT uuid::text FROM video_publications WHERE media_id=%s", media
            )
            publisher = _encoder(server, key)
            wait_for(
                lambda: source.db_value(
                    "SELECT live_state='live' FROM media_attachments WHERE id=%s", media
                ),
                desc="first native broadcast on air",
            )
            session, old_hash = source.db.conn.execute(
                "SELECT session_id,p2p_infohash FROM native_live_sessions WHERE media_id=%s AND ended_at IS NULL",
                (media,),
            ).fetchone()
            settings = tomllib.loads(original_config)
            original_media = Path(settings["media_dir"])
            recording_root = original_media / "native-live" / str(media) / session
            wait_for(
                lambda: len(list(recording_root.glob("segment*.ts"))) >= 4,
                desc="complete rolling segments and recording fragments exist",
                timeout=90,
            )
            recording = recording_root / "replay.mp4"
            assert recording.stat().st_size > 100_000
            assert "failure reason" not in asyncio.run(
                _announce(source.domain, old_hash)
            )

        checkpoint = None
        if paired:
            with step(
                "real rolling checkpoint serves missing primary segments with verified native browser decoding"
            ):
                mirrored_root = replica / "native-live" / str(media) / session

                def committed():
                    playlist = mirrored_root / "index.m3u8"
                    offset = mirrored_root / ".plamenu-recording-offset"
                    if not playlist.exists() or not offset.exists():
                        return None
                    names = [
                        line
                        for line in playlist.read_text().splitlines()
                        if line and not line.startswith("#")
                    ]
                    return (
                        (names, int(offset.read_text()))
                        if len(names) >= 4 and int(offset.read_text()) > 100_000
                        else None
                    )

                names, offset = wait_for(
                    committed,
                    desc="durable window and complete MP4 recording prefix",
                    timeout=90,
                )
                assert all((mirrored_root / name).is_file() for name in names)
                with step(
                    "replica-only rewind remains finite, decodable, ranged and usable without application scripts"
                ):
                    moved_recording = (
                        evidence / "temporarily-missing-primary-recording.mp4"
                    )
                    shutil.move(recording, moved_recording)
                    try:
                        rewind = f"{source.url}/media/live/{media}/rewind/{int(time.time()) // 60}/0.mp4"
                        response = requests.get(rewind, verify=False, timeout=45)
                        assert response.status_code == 200, (
                            response.text[:300] if response.status_code != 200 else ""
                        )
                        assert response.headers["Cache-Control"] == "private, no-store"
                        clip = evidence / "replica-only-rewind.mp4"
                        clip.write_bytes(response.content)
                        clip.chmod(0o600)
                        subprocess.run(
                            [
                                "ffmpeg",
                                "-hide_banner",
                                "-loglevel",
                                "error",
                                "-xerror",
                                "-i",
                                str(clip),
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
                            timeout=45,
                        )
                        partial = requests.get(
                            rewind,
                            headers={"Range": "bytes=0-63"},
                            verify=False,
                            timeout=20,
                        )
                        assert (
                            partial.status_code == 206
                            and partial.content == response.content[:64]
                        )
                        assert (
                            requests.get(
                                rewind,
                                headers={"Range": "bytes=999999999999-"},
                                verify=False,
                                timeout=20,
                            ).status_code
                            == 416
                        )
                        observed = subprocess.run(
                            [
                                "node",
                                "verify-replica-rewind.mjs",
                                f"{source.url}/videos/live/{media}/rewind",
                                str(evidence),
                            ],
                            cwd=config.PLAMENU_DIR / "accessibility",
                            capture_output=True,
                            text=True,
                            timeout=60,
                            check=False,
                        )
                        (evidence / "replica-rewind-browser.log").write_text(
                            observed.stdout + observed.stderr
                        )
                        (evidence / "replica-rewind-browser.log").chmod(0o600)
                        assert observed.returncode == 0, (
                            observed.stdout + observed.stderr
                        )
                        assert not list(mirrored_root.glob(".plamenu-rewind-*")), (
                            "pinned input links leaked after the build"
                        )
                    finally:
                        shutil.move(moved_recording, recording)
                lost = evidence / "missing-primary-segments"
                lost.mkdir(mode=0o700)
                originals = {}
                for name in names:
                    path = recording_root / name
                    originals[name] = _digest(mirrored_root / name)
                    assert _digest(path) == originals[name]
                    shutil.move(
                        path, lost / name
                    )  # preserve owned fault bytes as evidence
                    reply = requests.get(
                        f"{source.url}/media/hls/{media}/local/{session}/{name}",
                        verify=False,
                        timeout=20,
                    )
                    assert reply.status_code == 200
                    assert hashlib.sha256(reply.content).hexdigest() == originals[name]
                hashes = requests.get(
                    f"{source.url}/media/hls/{media}/sha256.json",
                    verify=False,
                    timeout=20,
                )
                assert hashes.status_code == 200
                assert all(
                    hashes.json().get(name) == digest
                    for name, digest in originals.items()
                )
                decoded = subprocess.run(
                    [
                        "node",
                        "verify-live-checkpoint.mjs",
                        f"{source.url}/web/statuses/{status}",
                        str(evidence),
                    ],
                    cwd=config.PLAMENU_DIR / "accessibility",
                    capture_output=True,
                    text=True,
                    timeout=75,
                    check=False,
                )
                (evidence / "live-browser.log").write_text(
                    decoded.stdout + decoded.stderr
                )
                (evidence / "live-browser.log").chmod(0o600)
                assert decoded.returncode == 0, decoded.stdout + decoded.stderr
                browser_results = json.loads(decoded.stdout.strip().splitlines()[-1])
                assert all(
                    response["status"] < 400
                    for result in browser_results
                    for response in result["responses"]
                )
                js = next(
                    result for result in browser_results if result["javaScriptEnabled"]
                )
                assert any(
                    Path(urlsplit(response["url"]).path).name in originals
                    for response in js["responses"]
                ), (
                    "decoded JS journey never requested a deliberately missing primary segment"
                )
                no_js = next(
                    result
                    for result in browser_results
                    if not result["javaScriptEnabled"]
                )
                assert any(
                    f"/media/live/{media}/stream.mp4" in response["url"]
                    for response in no_js["responses"]
                )
                # Direct HTTP checks above exercise every missing segment;
                # browser assertions additionally prove real JS/no-JS decode.
                checkpoint = {
                    "offset": offset,
                    "missing_segments": originals,
                    "browsers": browser_results,
                }

        with step(
            "crash only the owned source, quiesce its writer and archive DB plus whole roots"
        ):
            assert source._proc is not None and source._proc.poll() is None
            source._proc.kill()
            source._proc.wait(timeout=15)
            source._log.close()
            _stop_encoder(publisher)
            publisher = None
            # The pipe reader may flush its last complete fragment after the
            # server dies. Compare consecutive manifests before snapshotting.
            files = _manifest(original_media)
            time.sleep(1)
            assert _manifest(original_media) == files, (
                "orphaned media writer has not quiesced"
            )
            if paired:
                # Lose only this fixture's exact primary generation, retaining
                # it privately as forensic evidence. Recovery must use the
                # committed replica, not an accidental surviving source file.
                shutil.move(recording_root, evidence / "lost-primary-generation")
                files = _manifest(original_media)
            with (backup / "database.dump").open("wb") as output:
                subprocess.run(
                    [
                        "pg_dump",
                        "--format=custom",
                        "--no-owner",
                        "--dbname",
                        source.dsn,
                    ],
                    stdout=output,
                    check=True,
                    timeout=120,
                )
            roots = {"media": (original_media, files)}
            if paired:
                roots["replica"] = (replica, _manifest(replica))
                assert (
                    replica / "native-live" / str(media) / session / "replay.mp4"
                ).is_file()
            for name, (directory, manifest) in roots.items():
                with tarfile.open(backup / f"{name}.tar.gz", "w:gz") as archive:
                    archive.add(directory, arcname=name)
                (backup / f"{name}-manifest.json").write_text(
                    json.dumps(manifest, indent=2)
                )
            (escrow / "plamenu.toml").write_text(original_config)
            (escrow / "plamenu.toml").chmod(0o600)
            for path in backup.iterdir():
                path.chmod(0o600)
            retained_log = evidence / "source-server.log"
            retained_log.write_text((Path(source._dir.name) / "server.log").read_text())
            retained_log.chmod(0o600)
            source.stop()
            assert not original_media.exists()

        with step(
            "restore into fresh DB and directories, retaining original encrypted identities"
        ):
            with psycopg.connect(config.DB_DSN, autocommit=True) as connection:
                connection.execute(
                    sql.SQL("CREATE DATABASE {} TEMPLATE template0").format(
                        sql.Identifier(restored.db_name)
                    )
                )
            subprocess.run(
                [
                    "pg_restore",
                    "--exit-on-error",
                    "--no-owner",
                    "--dbname",
                    restored.dsn,
                    str(backup / "database.dump"),
                ],
                check=True,
                capture_output=True,
                timeout=120,
            )
            restored._dir = tempfile.TemporaryDirectory(
                prefix="plamenu-ap09-live-recovered-",
                dir=config.PLAMENU_DIR / "target/e2e/instances",
            )
            root = Path(restored._dir.name)
            recovered_config = original_config.replace(source.dsn, restored.dsn)
            for name, (directory, manifest) in roots.items():
                with tarfile.open(backup / f"{name}.tar.gz", "r:gz") as archive:
                    archive.extractall(root, filter="data")
                assert _manifest(root / name) == manifest
                recovered_config = recovered_config.replace(
                    str(directory), str(root / name)
                )
            restored._config_path.write_text(recovered_config)
            restored._config_path.chmod(0o600)
            assert "plaintext audit clean" in restored.cli(
                "federation", "keys", "audit"
            )
            restored._launch(120, append_log=False)
            assert api.get("/api/v1/accounts/verify_credentials")["id"]
            assert (
                restored.db_value(
                    "SELECT uuid::text FROM video_publications WHERE media_id=%s", media
                )
                == original_uuid
            )
            wait_for(
                lambda: restored.db_value(
                    "SELECT live_state='interrupted' FROM media_attachments WHERE id=%s",
                    media,
                ),
                desc="restored active session is interrupted, never phantom on-air",
            )
            old_child = f"{restored.url}/media/hls/{media}/local/{session}/index.m3u8"
            assert requests.get(old_child, verify=False, timeout=20).status_code == 404
            assert (
                asyncio.run(_announce(restored.domain, old_hash))["failure reason"]
                == "unknown info_hash"
            )

        with step(
            "old ingest credentials reconnect with PeerTube-compatible swarm identity and fenced segment URLs"
        ):
            publisher = _encoder(server, key)
            wait_for(
                lambda: restored.db_value(
                    "SELECT live_state='live' FROM media_attachments WHERE id=%s", media
                ),
                desc="original credentials reconnect recovered live identity",
            )
            generations = restored.db.conn.execute(
                "SELECT generation,ended_at IS NOT NULL,p2p_infohash "
                "FROM native_live_sessions WHERE media_id=%s ORDER BY generation",
                (media,),
            ).fetchall()
            assert len(generations) == 2 and [
                (row[0], row[1]) for row in generations
            ] == [(1, True), (2, False)]
            # PeerTube 8.3 derives its swarm from the stable Video UUID and
            # resolution, not a recording generation. Changing the hash
            # would break the unmodified remote player. Off-air announces
            # were refused above; new on-air resumes the compatible hash,
            # while exact generation paths continue to refuse old bytes.
            assert generations[1][2] == old_hash
            assert "failure reason" not in asyncio.run(
                _announce(restored.domain, generations[1][2])
            )
            assert requests.get(old_child, verify=False, timeout=20).status_code == 404
            new_session = restored.db_value(
                "SELECT session_id FROM native_live_sessions WHERE media_id=%s AND ended_at IS NULL",
                media,
            )
            new_root = root / "media/native-live" / str(media) / new_session
            wait_for(
                lambda: len(list(new_root.glob("segment*.ts"))) >= 3,
                desc="recovered encoder has complete independently recorded segments",
                timeout=90,
            )
            _stop_encoder(publisher)
            publisher = None

        with step(
            "both interrupted and clean recordings become independently playable PeerTube replay Videos"
        ):

            def episodes():
                rows = restored.db.conn.execute(
                    "SELECT ls.generation,ls.replay_media_id,m.status_id,v.uuid::text,m.file_name "
                    "FROM native_live_sessions ls LEFT JOIN media_attachments m ON m.id=ls.replay_media_id "
                    "LEFT JOIN video_publications v ON v.status_id=m.status_id WHERE ls.media_id=%s ORDER BY ls.generation",
                    (media,),
                ).fetchall()
                return rows if len(rows) == 2 and all(row[3] for row in rows) else None

            replays = wait_for(
                episodes, desc="both recovered replay publications finish", timeout=240
            )
            assert len({row[1] for row in replays}) == 2
            decoded = []
            for (
                generation,
                replay_media,
                replay_status,
                replay_uuid,
                filename,
            ) in replays:
                item = api.get_status(replay_status)["media_attachments"][0]
                response = requests.get(item["url"], verify=False, timeout=30)
                assert response.status_code == 200
                movie = evidence / f"episode-{generation}.mp4"
                movie.write_bytes(response.content)
                movie.chmod(0o600)
                subprocess.run(
                    [
                        "ffmpeg",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-xerror",
                        "-i",
                        str(movie),
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
                    timeout=60,
                )

                def imported(replay_uuid=replay_uuid):
                    try:
                        video = pt.video(replay_uuid)
                    except peertube.PeertubeError:
                        return None
                    return (
                        video
                        if video["state"]["id"] == 1
                        and video["files"]
                        and not video["isLive"]
                        else None
                    )

                remote = wait_for(
                    imported,
                    desc="surviving PeerTube imports recovered replay",
                    timeout=180,
                )
                file_url = remote["files"][0]["fileUrl"]
                partial = requests.get(
                    file_url, headers={"Range": "bytes=0-63"}, verify=False, timeout=30
                )
                assert (
                    partial.status_code == 206
                    and partial.content == response.content[:64]
                )
                if paired:
                    assert _digest(root / "media" / filename) == _digest(
                        root / "replica" / filename
                    )
                    # Delete only this fixture's exact durable movie key;
                    # preserve its archive and prove streamed replica failover.
                    (root / "media" / filename).unlink()
                    fallback = requests.get(
                        file_url,
                        headers={"Range": "bytes=0-63"},
                        verify=False,
                        timeout=30,
                    )
                    assert (
                        fallback.status_code == 206
                        and fallback.content == partial.content
                    )
                # Replay keys are charged to their original broadcast's
                # ledger, not the independently published replay media ID.
                assert restored.db_value(
                    "SELECT count(*)=1 FROM video_storage_reservations WHERE file_name=%s",
                    filename,
                )
                api.delete_status(replay_status)
                wait_for(
                    lambda filename=filename: restored.db_value(
                        "SELECT count(*)=0 FROM video_storage_reservations WHERE file_name=%s",
                        filename,
                    ),
                    desc="recovered replay cleanup acknowledges its storage ledger",
                )
                assert not (root / "media" / filename).exists()
                if paired:
                    assert not (root / "replica" / filename).exists()
                decoded.append(
                    {
                        "generation": generation,
                        "replay_uuid": replay_uuid,
                        "bytes": len(response.content),
                        "sha256": _digest(movie),
                    }
                )
            assert (
                restored.db_value(
                    "SELECT uuid::text FROM video_publications WHERE media_id=%s", media
                )
                == original_uuid
            )
            wait_for(
                lambda: (
                    not (root / "media/native-live" / str(media) / session).exists()
                ),
                desc="interrupted recording scratch is cleaned only after successful replay",
            )
            if paired:
                assert not (
                    root / "replica/native-live" / str(media) / session
                ).exists()
            result = {
                "candidate_sha256": _digest(ephemeral.binary_path()),
                "paired_roots": paired,
                "backup_sha256": {
                    path.name: _digest(path) for path in backup.iterdir()
                },
                "original_live_uuid": original_uuid,
                "episodes": decoded,
                "live_checkpoint_faults": checkpoint,
                "checks": "fresh DB/root restore, original keys, interrupted state, off-air tracker refusal, generation-fenced HLS, compatible reconnect, decoded independent replays, PeerTube ranges, acknowledged cleanup",
            }
            (evidence / "result.json").write_text(json.dumps(result, indent=2))
            (evidence / "result.json").chmod(0o600)
            print(f"Private live recovery evidence: {evidence}")
    finally:
        _stop_encoder(publisher)
        try:
            active = (
                restored
                if restored._proc is not None and restored._proc.poll() is None
                else source
            )
            if (
                channel is not None
                and active._proc is not None
                and active._proc.poll() is None
            ):
                api.post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
            if subscribed:
                pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        finally:
            for name, instance in (("source", source), ("restored", restored)):
                if instance._dir is not None:
                    log = Path(instance._dir.name) / "server.log"
                    if log.exists():
                        retained = evidence / f"{name}-server.log"
                        retained.write_text(log.read_text())
                        retained.chmod(0o600)
                instance.stop()
            browser.close()
