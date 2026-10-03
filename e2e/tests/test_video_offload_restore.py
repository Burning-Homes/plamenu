"""Interrupted offload claim restored into a fresh DB and media root.

Only owned origin/worker processes, uniquely named databases and the fixture
port are affected. Consistent backups, source films and escrow are retained.
"""

import json
import os
import socket
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path

import psycopg
import pytest
import requests
import tomllib
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from psycopg import sql
from tests.test_video_backup_restore import _digest, _manifest, _stop_process
from tests.test_video_offload import (
    Processor,
    _encoder,
    _freeze,
    _gone,
    _source_path,
    _submit,
)


@pytest.mark.skipif(
    os.environ.get("AP09_OFFLOAD_RESTORE_VERIFY") != "1",
    reason="explicit private offload restore drill",
)
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="restored origin keys publish recovered private native processing to actual PeerTube",
)
def test_interrupted_offload_restores_fenced_source_claim_and_peer_publication(
    monkeypatch,
):
    assert peertube.reachable(), "actual PeerTube is required"
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "owned fixture port is occupied"
        )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-offload-restore"
    artifacts.mkdir(parents=True, exist_ok=True)
    label = unique("offloadrestore").replace("-", "_")
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    backup = evidence / "consistent-backup"
    backup.mkdir(mode=0o700)
    origin = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    restored = ephemeral.Instance(
        domain=origin.domain,
        bind=origin.bind,
        db_name=f"plamenu_ap09_{label}_recovered",
    )
    active = origin
    processor = channel = api = pt = published = None
    subscribed = False
    receipts = {}
    try:
        with step(
            "origin crash interrupts a real remote encode without losing its committed private source"
        ):
            origin.start()
            _freeze(origin, evidence, monkeypatch)
            _stop_process(origin)
            processor = Processor(evidence / "processor", tls=True)
            origin._config_path.write_text(
                origin._config_path.read_text()
                + "\n[native_video]\nmax_upload_mib=512\njobs=1\n"
                + f"\n[[video_offload]]\nurl={json.dumps(processor.url + '/')}\ntoken={json.dumps(processor.token)}\n"
            )
            origin._config_path.chmod(0o600)
            origin._launch(120, append_log=True)
            origin.apply_suite_settings()
            with psycopg.connect(origin.dsn, autocommit=True) as connection:
                connection.execute(
                    "UPDATE instance_settings SET media_video_preset='veryslow'"
                )
            time.sleep(6)
            user, api = origin.user_api("offloadrecover")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("recoveredfilms")[:30],
                display_name="Recovered processing",
            )
            pt = peertube.root()
            handle = f"{channel['username']}@{origin.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: origin.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube follows before backup",
            )
            source = evidence / "source.webm"
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=1280x720:rate=15",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-t",
                    "60",
                    "-c:v",
                    "libvpx-vp9",
                    "-deadline",
                    "realtime",
                    "-cpu-used",
                    "8",
                    "-threads",
                    "2",
                    "-c:a",
                    "libopus",
                    "-y",
                    str(source),
                ],
                check=True,
                capture_output=True,
                timeout=180,
            )
            title = f"Recovered native offload {label}"
            mid, _ = _submit(origin, user, channel, source, title)
            encoder = wait_for(
                lambda: _encoder(processor),
                desc="real remote encoder before origin crash",
                timeout=30,
            )
            assert origin.db_value(
                "SELECT state='processing' FROM video_drafts WHERE media_id=%s", mid
            )
            generation = origin.db_value(
                "SELECT generation FROM video_drafts WHERE media_id=%s", mid
            )
            candidate = _digest(Path(f"/proc/{origin._proc.pid}/exe"))
            assert _digest(Path(f"/proc/{processor.proc.pid}/exe")) == candidate
            assert _digest(_source_path(origin, mid)) == _digest(source)
            # Only the owned origin is killed. Remote work must expire on its
            # real lease, not a test DB update or an operator cancellation.
            origin._proc.kill()
            origin._proc.wait(timeout=15)
            origin._log.close()
            assert _digest(_source_path(origin, mid)) == _digest(source)

        with step(
            "snapshot quiescent origin data and separately private configuration escrow"
        ):
            original_config = origin._config_path.read_text()
            settings = tomllib.loads(original_config)
            original_media = Path(settings["media_dir"])
            files = _manifest(original_media)
            with (backup / "database.dump").open("wb") as output:
                subprocess.run(
                    [
                        "pg_dump",
                        "--format=custom",
                        "--no-owner",
                        "--dbname",
                        origin.dsn,
                    ],
                    check=True,
                    stdout=output,
                    stderr=subprocess.PIPE,
                    timeout=120,
                )
            with tarfile.open(backup / "media.tar.gz", "w:gz") as archive:
                archive.add(original_media, arcname="media")
            (backup / "manifest.json").write_text(json.dumps(files, indent=2))
            escrow = evidence / "origin-configuration-escrow.toml"
            escrow.write_text(original_config)
            escrow.chmod(0o600)
            reservations = origin.db_value(
                "SELECT jsonb_agg(to_jsonb(r) ORDER BY file_name) FROM video_storage_reservations r"
            )
            wait_for(
                lambda: processor.health_or_none()["active_jobs"] == 0,
                desc="orphan offload expires without origin renewal",
                timeout=45,
            )
            wait_for(
                lambda: _gone(encoder),
                desc="orphan encoder cannot survive lease expiry",
                timeout=10,
            )
            assert not list((processor.root / "scratch/tmp").iterdir())

        with step(
            "restore original identity and interrupted claim into a fresh DB/root"
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
                prefix=f"plamenu-{label}-recovered-",
                dir=config.PLAMENU_DIR / "target/e2e/instances",
            )
            root = Path(restored._dir.name)
            with tarfile.open(backup / "media.tar.gz", "r:gz") as archive:
                archive.extractall(root, filter="data")
            assert _manifest(root / "media") == files
            recovered = original_config.replace(origin.dsn, restored.dsn).replace(
                str(original_media), str(root / "media")
            )
            restored._config_path.write_text(recovered)
            restored._config_path.chmod(0o600)
            wrong = root / "wrong-secret.toml"
            wrong.write_text(
                recovered.replace(
                    settings["encryption_secret"], "wrong-offload-recovery-secret"
                )
            )
            wrong.chmod(0o600)
            failure = subprocess.run(
                [
                    str(ephemeral.binary_path()),
                    "--config",
                    str(wrong),
                    "federation",
                    "keys",
                    "audit",
                ],
                capture_output=True,
                timeout=30,
                check=False,
            )
            assert failure.returncode != 0, (
                "wrong origin key must not decrypt restored federation identity"
            )
            assert "plaintext audit clean" in restored.cli(
                "federation", "keys", "audit"
            )
            assert (
                restored.db_value(
                    "SELECT jsonb_agg(to_jsonb(r) ORDER BY file_name) FROM video_storage_reservations r"
                )
                == reservations
            )
            restored._launch(120, append_log=False)
            active = restored
            assert _digest(Path(f"/proc/{restored._proc.pid}/exe")) == candidate
            assert api.get("/api/v1/accounts/verify_credentials")["id"] == str(
                origin.db_value(
                    "SELECT id FROM accounts WHERE username=%s", user.username
                )
            )
            assert _digest(_source_path(restored, mid)) == _digest(source)
            assert (
                requests.get(
                    f"{restored.url}/media/hls/{mid}/master.m3u8",
                    verify=False,
                    timeout=30,
                ).status_code
                == 404
            )
            # The original 120s DB claim expires naturally. No timestamps or
            # generations are rewritten by this drill to force an early retry.
            wait_for(
                lambda: restored.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s", mid
                ),
                desc="restored expired claim re-encodes through native TLS processor",
                timeout=420,
            )
            assert restored.db_value(
                "SELECT generation>%s FROM video_drafts WHERE media_id=%s",
                generation,
                mid,
            )
            assert processor.health_or_none()["completed_jobs"] >= 1
            text = (Path(restored._dir.name) / "server.log").read_text()
            assert "native video offload output verified" in text
            assert "private video processor unavailable or invalid" not in text
            assert _digest(_source_path(restored, mid)) == _digest(source)

        with step(
            "only the recovered origin publishes and actual PeerTube accepts its original signer"
        ):
            published = api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=str(mid),
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
                desc="PeerTube accepts recovered signed publication",
            )
            assert pt.video(remote["uuid"])["streamingPlaylists"]
            media = published["media_attachments"][0]
            content = requests.get(media["url"], verify=False, timeout=30)
            content.raise_for_status()
            decoded = evidence / "recovered.mp4"
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
                timeout=120,
            )
            ranged = requests.get(
                media["url"], headers={"Range": "bytes=17-80"}, verify=False, timeout=30
            )
            assert (
                ranged.status_code == 206 and ranged.content == content.content[17:81]
            )
            assert restored.db_value(
                "SELECT count(*)=1 FROM video_publications WHERE media_id=%s", mid
            )
            receipts = {
                "candidate_sha256": candidate,
                "source_sha256": _digest(source),
                "backup_sha256": {p.name: _digest(p) for p in backup.iterdir()},
                "original_generation": generation,
                "recovered_generation": restored.db_value(
                    "SELECT generation FROM video_drafts WHERE media_id=%s", mid
                ),
                "fresh_database": True,
                "fresh_media_root": True,
                "orphan_encoder_expired": True,
                "private_source_preserved": True,
                "peer_uuid": remote["uuid"],
                "worker": processor.health_or_none(),
            }
            (evidence / "result.json").write_text(json.dumps(receipts, indent=2))
            print(f"Protected offload recovery evidence: {evidence}")
    finally:
        try:
            for name, instance in [("origin", origin), ("restored", restored)]:
                if instance._dir is not None:
                    log = Path(instance._dir.name) / "server.log"
                    if log.exists():
                        (evidence / f"{name}.log").write_text(log.read_text())
            if (
                active._proc is not None
                and active._proc.poll() is None
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
            if processor is not None:
                processor.stop()
            origin.stop()
            restored.stop()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
