"""Warm real PeerTube cache, then revoke its local channel and signed Video.

Only the uniquely owned consumer DB/server and the two created PeerTube videos
are changed. Remote actor suspension is local moderation of the fixture copy,
not mutation of the standing PeerTube administrator or another user's video.
"""

import hashlib
import json
import socket
import subprocess
import tarfile
import tempfile
from pathlib import Path
from urllib.parse import urljoin

import psycopg
import pytest
import requests
import tomllib
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from psycopg import sql
from tests.test_video_backup_restore import _digest, _manifest, _stop_process
from tests.test_video_offload import _freeze


def _retain_fixture_instance_identity(consumer):
    """One canonical test server retains its identity across throwaway DBs.

    PeerTube legitimately caches /actor keys. Recreating a different server
    under that same URI is NOT recovery and yields an expected invalid
    signature. Only encrypted instance-key rows and their wrapping secret are
    escrowed here; no standing identity, accounts or remote cache is imported.
    """
    assert consumer.domain == ephemeral.DOMAIN
    directory = config.PLAMENU_DIR / "target/e2e/video-remote-cache-identity"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    directory.chmod(0o700)
    identity = directory / "doomed-instance-identity.json"
    _stop_process(consumer)
    settings = tomllib.loads(consumer._config_path.read_text())
    if identity.exists():
        assert identity.stat().st_mode & 0o077 == 0, (
            "private identity escrow permissions"
        )
        saved = json.loads(identity.read_text())
        assert saved["domain"] == consumer.domain and saved["version"] == 1
        with psycopg.connect(consumer.dsn) as connection:
            connection.execute("DELETE FROM actor_keys WHERE owner_kind='instance'")
            for row in saved["encrypted_keys"]:
                assert row["owner_kind"] == "instance" and row["account_id"] is None
                assert row["controller_uri"] == consumer.url + "/actor"
                connection.execute(
                    "INSERT INTO actor_keys SELECT * FROM json_populate_record(NULL::actor_keys, %s::json)",
                    (json.dumps(row),),
                )
        consumer._config_path.write_text(
            consumer._config_path.read_text().replace(
                settings["encryption_secret"], saved["wrapping_secret"]
            )
        )
        consumer._config_path.chmod(0o600)
    else:
        keys = consumer.db_value(
            "SELECT jsonb_agg(to_jsonb(k) ORDER BY id) FROM actor_keys k WHERE owner_kind='instance'"
        )
        assert keys, "startup must have initialized the test server identity"
        # Exclusive creation prevents replacing an existing identity silently.
        with identity.open("x") as output:
            identity.chmod(0o600)
            json.dump(
                {
                    "version": 1,
                    "domain": consumer.domain,
                    "wrapping_secret": settings["encryption_secret"],
                    "encrypted_keys": keys,
                },
                output,
            )
    assert "plaintext audit clean" in consumer.cli("federation", "keys", "audit")
    consumer._launch(120, append_log=True)


def _get(url, **kwargs):
    return requests.get(url, verify=False, timeout=30, **kwargs)


def _child(playlist, base, needle):
    return urljoin(
        base,
        next(
            line
            for line in playlist.splitlines()
            if line and not line.startswith("#") and needle in line
        ),
    )


def _warm(attachment, *, live=False):
    master = attachment["hls"]["master"]
    response = _get(master)
    assert response.status_code == 200, (
        master,
        response.status_code,
        response.text[:200],
    )
    rendition = _child(response.text, master, "/live/" if live else "/pl?")
    response = _get(rendition)
    assert response.status_code == 200
    segment = _child(response.text, rendition, "/live/" if live else "/seg?")
    first, second = _get(segment), _get(segment)
    assert (
        first.status_code == second.status_code == 200
        and first.content == second.content
    )
    assert first.content
    checksums = attachment["hls"]["p2p"]["sha256_url"]
    response = _get(checksums)
    assert response.status_code == 200
    assert response.json()
    urls = [master, rendition, segment, checksums]
    if not live:
        response = _get(attachment["url"], headers={"Range": "bytes=0-63"})
        assert response.status_code == 206 and len(response.content) == 64
        urls.append(attachment["url"])
    return {
        "urls": urls,
        "segment": segment,
        "sha256": hashlib.sha256(first.content).hexdigest(),
        "segment_bytes": len(first.content),
    }


def _refused(record):
    for url in record["urls"]:
        response = _get(url, headers={"Range": "bytes=0-63"})
        assert response.status_code == 404, (url, response.status_code)
        assert response.headers["Cache-Control"] == "private, no-store"


def _restore_warm_cache(source, restored, evidence, candidate):
    """Quiescent paired cache/debt recovery; never reuse the original DB/root."""
    _stop_process(source)
    settings = tomllib.loads(source._config_path.read_text())
    original_config = source._config_path.read_text()
    backup = evidence / "consistent-backup"
    backup.mkdir(mode=0o700)
    roots = {
        "media": Path(settings["media_dir"]),
        "replica": Path(settings["media_mirror"]["directory"]),
    }
    manifests = {name: _manifest(path) for name, path in roots.items()}
    ledger_sql = "SELECT jsonb_agg(to_jsonb(r) ORDER BY file_name) FROM media_cleanup_jobs r WHERE remote_cache_bytes>0"
    debt = source.db_value(ledger_sql)
    assert debt, "the recovery must carry actual charged failed-deletion debt"
    cache_rows = source.db_value(
        "SELECT jsonb_agg(to_jsonb(r) ORDER BY cache_file) FROM media_hls_segments r"
    )
    with (backup / "database.dump").open("wb") as output:
        subprocess.run(
            ["pg_dump", "--format=custom", "--no-owner", "--dbname", source.dsn],
            check=True,
            stdout=output,
            stderr=subprocess.PIPE,
            timeout=120,
        )
    for name, path in roots.items():
        with tarfile.open(backup / f"{name}.tar.gz", "w:gz") as archive:
            archive.add(path, arcname=name)
    (backup / "manifest.json").write_text(json.dumps(manifests, indent=2))
    escrow = evidence / "consumer-configuration-escrow.toml"
    escrow.write_text(original_config)
    escrow.chmod(0o600)
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
        prefix="plamenu-cache-recovered-",
        dir=config.PLAMENU_DIR / "target/e2e/instances",
    )
    root = Path(restored._dir.name)
    recovered = original_config.replace(source.dsn, restored.dsn)
    for name, path in roots.items():
        with tarfile.open(backup / f"{name}.tar.gz", "r:gz") as archive:
            archive.extractall(root, filter="data")
        assert _manifest(root / name) == manifests[name]
        recovered = recovered.replace(str(path), str(root / name))
    restored._config_path.write_text(recovered)
    restored._config_path.chmod(0o600)
    wrong = root / "wrong-secret.toml"
    wrong.write_text(
        recovered.replace(
            settings["encryption_secret"], "wrong-remote-cache-recovery-secret"
        )
    )
    wrong.chmod(0o600)
    refusal = subprocess.run(
        [str(ephemeral.BINARY), "--config", str(wrong), "federation", "keys", "audit"],
        capture_output=True,
        timeout=30,
        check=False,
    )
    assert refusal.returncode != 0
    assert "plaintext audit clean" in restored.cli("federation", "keys", "audit")
    assert restored.db_value(ledger_sql) == debt
    assert (
        restored.db_value(
            "SELECT jsonb_agg(to_jsonb(r) ORDER BY cache_file) FROM media_hls_segments r"
        )
        == cache_rows
    )
    restored._launch(120, append_log=False)
    assert _digest(Path(f"/proc/{restored._proc.pid}/exe")) == candidate
    return {
        "fresh_database": True,
        "fresh_paired_roots": True,
        "wrong_secret_refused": True,
        "charged_cleanup_debt_preserved": True,
        "warm_rows_preserved": True,
        "backup_sha256": {path.name: _digest(path) for path in backup.iterdir()},
    }


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="Inbound cache-authority and signed-delete refinement on a restored consumer; outbound native authority is covered separately and does not exercise a remote-cache path.",
)
@pytest.mark.parametrize("fresh", [False, True])
def test_real_remote_vod_live_cache_refuses_current_channel_and_signed_delete(
    fresh, monkeypatch
):
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8421)) != 0, (
            "disposable fixture port is occupied"
        )
    label = unique("remotecache").replace("-", "_")
    consumer = ephemeral.Instance(
        domain=ephemeral.DOMAIN, bind=ephemeral.BIND, db_name=f"plamenu_ap09_{label}"
    )
    source = consumer
    restored = ephemeral.Instance(
        domain=consumer.domain,
        bind=consumer.bind,
        db_name=f"plamenu_ap09_{label}_recovered",
    )
    artifacts = config.PLAMENU_DIR / "target/e2e/video-remote-cache"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    pt = peertube.root()
    vod = live = suspended_channel = domain_block = None
    owned_abuse_ids = []
    records = {}
    debt_drill = None
    recovery = None
    deleted = False
    try:
        consumer.start()
        _freeze(consumer, evidence, monkeypatch)
        _retain_fixture_instance_identity(consumer)
        _stop_process(consumer)
        replica = Path(consumer._dir.name) / "replica"
        replica.mkdir()
        consumer._config_path.write_text(
            consumer._config_path.read_text()
            + f'\n[media_mirror]\ndirectory = "{replica}"\nbandwidth_mib_per_second = 32\nmin_free_gib = 0\n'
        )
        consumer._config_path.chmod(0o600)
        consumer._launch(120, append_log=True)
        candidate_sha = _digest(Path(f"/proc/{consumer._proc.pid}/exe"))
        consumer.apply_suite_settings()
        _, api = consumer.user_api("remoteviewer")
        _, admin = consumer.admin("remoteoperator")
        root = api.resolve_account(f"root@{config.PEERTUBE_DOMAIN}")
        assert root
        api.follow(root["id"])
        with step("warm real PeerTube film playlists, integrity and exact-range cache"):
            movie = peertube.make_sample_mp4(
                evidence / "origin-fixture.mp4", seconds=8, size="640x360"
            )
            vod = pt.upload_video(movie, f"Warm remote film {label}")
            pt.wait_for_hls(vod["uuid"])

            def origin_transcodes_done():
                for state in (
                    "active",
                    "waiting",
                    "waiting-children",
                    "prioritized",
                    "delayed",
                ):
                    jobs = pt.get(
                        f"/api/v1/jobs/{state}", jobType="video-transcoding", count=100
                    )
                    assert jobs["total"] <= 100, (
                        "fixture job inspection must not silently truncate"
                    )
                    if any(
                        job["data"].get("videoUUID") == vod["uuid"]
                        for job in jobs["data"]
                    ):
                        return False
                return True

            wait_for(
                origin_transcodes_done,
                desc="owned PeerTube transcoding jobs finish rotating files",
                timeout=180,
                interval=3,
            )

            # Advertised streamingPlaylists can precede the actual asset. This
            # gate checks the real origin before the first consumer fetch;
            # never clear the product's persistent fetch budget to retry it.
            def origin_ready():
                details = pt.video(vod["uuid"])
                origin_master = details["streamingPlaylists"][0]["playlistUrl"]
                response = _get(origin_master)
                return (
                    response
                    if response.status_code == 200
                    and response.text.startswith("#EXTM3U")
                    else None
                )

            origin_response = wait_for(
                origin_ready, desc="actual origin master asset is readable", timeout=90
            )
            (evidence / "origin-master.m3u8").write_text(origin_response.text)
            status = wait_for(
                lambda: api.resolve_status(pt.ap_id(vod["uuid"])),
                desc="native consumer imports remote film",
            )
            film = status["media_attachments"][0]
            media = int(film["id"])
            wait_for(
                lambda: consumer.db_value(
                    "SELECT hls_master_url=%s FROM media_attachments WHERE id=%s",
                    origin_response.url,
                    media,
                ),
                desc="current readable origin master Update arrives",
                timeout=90,
            )
            records["vod"] = _warm(film)
            retained_keys = [
                row[0]
                for row in consumer.db.conn.execute(
                    "SELECT cache_file FROM media_hls_segments WHERE media_id=%s",
                    (media,),
                ).fetchall()
            ]
            assert retained_keys, "actual disk cache rows are required"

        with step(
            "consumer crash/restart retains exact acknowledged remote cache bytes"
        ):
            consumer.restart_process_after_crash()
            assert _digest(Path(f"/proc/{consumer._proc.pid}/exe")) == candidate_sha
            assert (
                hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                == records["vod"]["sha256"]
            )
            assert set(retained_keys) <= {
                row[0]
                for row in consumer.db.conn.execute(
                    "SELECT cache_file FROM media_hls_segments WHERE media_id=%s",
                    (media,),
                ).fetchall()
            }

        with step(
            "actual paired-disk expiry retains failed cleanup debt and never reuses retired keys"
        ):
            key, charged = consumer.db.conn.execute(
                "SELECT cache_file,bytes FROM media_hls_segments WHERE media_id=%s AND cache_file LIKE 'hls%%'",
                (media,),
            ).fetchone()
            primary = Path(consumer._dir.name) / "media"
            original = replica / key
            saved = evidence / "blocked-replica-cache.mp4"
            original.rename(saved)
            original.mkdir()  # Only this exact owned cache key cannot be removed.
            try:
                consumer.db.conn.execute(
                    "UPDATE instance_settings SET media_video_retention_days=1"
                )
                consumer.db.conn.execute(
                    "UPDATE media_hls_segments SET cached_at=now()-interval '3 days',last_served_at=now()-interval '3 days' WHERE media_id=%s",
                    (media,),
                )
                maintenance = consumer.cli("media", "expire-caches")
                (evidence / "retention-cli.log").write_text(maintenance)
                wait_for(
                    lambda: consumer.db_value(
                        "SELECT remote_cache_bytes=%s AND attempts>0 FROM media_cleanup_jobs WHERE file_name=%s",
                        charged,
                        key,
                    ),
                    desc="failed replica deletion remains revoked and charged",
                    timeout=30,
                )
                assert _get(f"{consumer.url}/media/{key}").status_code == 404
                assert consumer.db_value(
                    "SELECT count(*)=0 FROM media_hls_segments WHERE media_id=%s AND cache_file=%s",
                    media,
                    key,
                )
                assert (
                    hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                    == records["vod"]["sha256"]
                )
                replacement = consumer.db_value(
                    "SELECT cache_file FROM media_hls_segments WHERE media_id=%s AND cache_file LIKE 'hls%%'",
                    media,
                )
                assert replacement != key
                assert (primary / replacement).is_file() and (
                    replica / replacement
                ).is_file()
                assert consumer.db_value(
                    "SELECT remote_cache_bytes=%s FROM media_cleanup_jobs WHERE file_name=%s",
                    charged,
                    key,
                )
                if fresh:
                    with step(
                        "restore warm paired caches and failed-deletion charge into fresh DB/roots"
                    ):
                        recovery = _restore_warm_cache(
                            consumer, restored, evidence, candidate_sha
                        )
                        consumer = restored
                        primary = Path(consumer._dir.name) / "media"
                        replica = Path(consumer._dir.name) / "replica"
                        original = replica / key
                        assert original.is_dir() and not any(original.iterdir())
                        assert api.get("/api/v1/accounts/verify_credentials")["id"]
                        assert _get(f"{consumer.url}/media/{key}").status_code == 404
                        assert consumer.db_value(
                            "SELECT remote_cache_bytes=%s FROM media_cleanup_jobs WHERE file_name=%s",
                            charged,
                            key,
                        )
                        assert (
                            hashlib.sha256(
                                _get(records["vod"]["segment"]).content
                            ).hexdigest()
                            == records["vod"]["sha256"]
                        )
                        assert (primary / replacement).is_file() and (
                            replica / replacement
                        ).is_file()
            finally:
                assert original.is_dir() and not any(original.iterdir())
                original.rmdir()
                saved.rename(original)
            consumer.db.conn.execute(
                "UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=%s", (key,)
            )
            wait_for(
                lambda: consumer.db_value(
                    "SELECT count(*)=0 FROM media_cleanup_jobs WHERE file_name=%s", key
                ),
                desc="repaired replica deletion is actually acknowledged",
                timeout=30,
            )
            assert not (primary / key).exists() and not (replica / key).exists()
            assert (primary / replacement).is_file() and (
                replica / replacement
            ).is_file()
            assert (
                hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                == records["vod"]["sha256"]
            )
            debt_drill = {
                "old_key": key,
                "new_key": replacement,
                "charged_bytes": charged,
                "failed_delete_no_refund": True,
                "refill_does_not_reuse_key": True,
                "both_roots_acknowledged": True,
            }
            retained_keys = [
                row[0]
                for row in consumer.db.conn.execute(
                    "SELECT cache_file FROM media_hls_segments WHERE media_id=%s",
                    (media,),
                ).fetchall()
            ]

        live = pt.create_live(f"Warm remote live {label}", latency_mode=1)
        with peertube.broadcast(pt, live["id"], size="320x180", rate=15):
            with step(
                "warm actual on-air PeerTube segment and rolling-integrity caches"
            ):

                def on_air():
                    item = api.resolve_status(pt.ap_id(live["uuid"]))
                    return (
                        item
                        if item
                        and item["media_attachments"][0].get("live", {}).get("state")
                        == "live"
                        else None
                    )

                live_status = wait_for(
                    on_air,
                    desc="native consumer sees actual PeerTube on air",
                    timeout=90,
                )
                records["live"] = _warm(live_status["media_attachments"][0], live=True)
            with step(
                "current verified remote channel suspension refuses every warm cache lane"
            ):
                channel = consumer.db_value(
                    "SELECT channel_id FROM remote_video_channels WHERE status_id=%s",
                    int(status["id"]),
                )
                assert channel is not None
                assert (
                    consumer.db_value(
                        "SELECT channel_id FROM remote_video_channels WHERE status_id=%s",
                        int(live_status["id"]),
                    )
                    == channel
                )
                admin.post(
                    f"/api/v1/admin/accounts/{channel}/action",
                    type="suspend",
                    text="Owned warm-cache authority drill",
                )
                suspended_channel = channel
                for record in records.values():
                    _refused(record)
                admin.post(f"/api/v1/admin/accounts/{channel}/unsuspend")
                suspended_channel = None
                assert (
                    hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                    == records["vod"]["sha256"]
                )
                # This old segment remains inside the real origin's DVR window.
                assert (
                    hashlib.sha256(_get(records["live"]["segment"]).content).hexdigest()
                    == records["live"]["sha256"]
                )

            with step(
                "actual forwarded video report reaches PeerTube, without itself granting takedown authority"
            ):
                comment = f"Owned AP09 cache abuse propagation {label}"
                report = api.report(
                    status["account"]["id"],
                    comment=comment,
                    forward=True,
                    status_ids=[status["id"]],
                )
                assert (
                    admin.get(f"/api/v1/admin/reports/{report['id']}")["comment"]
                    == comment
                )

                def accepted_report():
                    response = pt.get(
                        "/api/v1/abuses", search=label, count=100, sort="-createdAt"
                    )
                    assert response["total"] <= 100
                    return [
                        item
                        for item in response["data"]
                        if item["reason"] == comment
                        and (item.get("video") or {}).get("uuid") == vod["uuid"]
                    ]

                actual_reports = wait_for(
                    accepted_report,
                    desc="actual PeerTube video abuse receives signed forwarded Flag",
                    timeout=90,
                )
                owned_abuse_ids = [
                    item["id"]
                    for item in pt.get("/api/v1/abuses", search=label, count=100)[
                        "data"
                    ]
                    if item["reason"] == comment
                ]
                assert actual_reports
                admin.post(f"/api/v1/admin/reports/{report['id']}/resolve")
                # Filing/resolving a report must not secretly suspend an actor,
                # revoke a public video or expose the reporter to the origin.
                assert (
                    hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                    == records["vod"]["sha256"]
                )
                assert all(
                    item["reporterAccount"]
                    and item["reporterAccount"]["name"]
                    != api.get("/api/v1/accounts/verify_credentials")["username"]
                    for item in actual_reports
                )

            with step(
                "operator media block refuses every warm VOD/live lane and survives an actual consumer restart"
            ):
                block = admin.post(
                    "/api/v1/admin/domain_blocks",
                    domain=config.PEERTUBE_DOMAIN,
                    severity="noop",
                    reject_media="true",
                    private_comment=f"Owned AP09 media-only block {label}",
                )
                domain_block = block["id"]
                for record in records.values():
                    _refused(record)
                consumer.restart_process_after_crash()
                assert _digest(Path(f"/proc/{consumer._proc.pid}/exe")) == candidate_sha
                for record in records.values():
                    _refused(record)
                wait_for(
                    lambda: consumer.db_value(
                        "SELECT count(*)=0 FROM media_hls_segments WHERE media_id=%s",
                        media,
                    ),
                    desc="operator block revokes actual warm VOD cache rows",
                    timeout=45,
                )
                wait_for(
                    lambda: all(
                        not (Path(consumer._dir.name) / "media" / key).exists()
                        and not (replica / key).exists()
                        for key in retained_keys
                    ),
                    desc="operator block acknowledges cache removal on both roots",
                    timeout=45,
                )
                admin.delete(f"/api/v1/admin/domain_blocks/{domain_block}")
                domain_block = None
                assert (
                    hashlib.sha256(_get(records["vod"]["segment"]).content).hexdigest()
                    == records["vod"]["sha256"]
                )
                assert _get(records["live"]["urls"][0]).status_code == 200
                retained_keys = [
                    row[0]
                    for row in consumer.db.conn.execute(
                        "SELECT cache_file FROM media_hls_segments WHERE media_id=%s",
                        (media,),
                    ).fetchall()
                ]

        with step(
            "actual signed live end revokes warm child segments rather than serving old frames"
        ):
            wait_for(
                lambda: consumer.db_value(
                    "SELECT live_state<>'live' FROM media_attachments WHERE id=%s",
                    int(live_status["media_attachments"][0]["id"]),
                ),
                desc="signed PeerTube live end arrives",
                timeout=120,
            )
            _refused(records["live"])
        with step(
            "actual PeerTube signed Delete revokes disk ranges and removes acknowledged cache rows"
        ):
            pt.delete_video(vod["id"])
            deleted = True
            wait_for(
                lambda: api.get_status_or_none(status["id"]) is None,
                desc="signed remote Video Delete arrives",
                timeout=120,
            )
            _refused(records["vod"])
            wait_for(
                lambda: consumer.db_value(
                    "SELECT count(*)=0 FROM media_hls_segments WHERE media_id=%s", media
                ),
                desc="deleted remote cache ledger rows retire",
                timeout=45,
            )
            wait_for(
                lambda: all(
                    not (Path(consumer._dir.name) / "media" / key).exists()
                    for key in retained_keys
                ),
                desc="acknowledged cleanup removes actual cached files",
                timeout=45,
            )
            result = {
                "candidate_sha256": candidate_sha,
                "records": records,
                "retired_cache_keys": retained_keys,
                "consumer_restart": True,
                "channel_suspension": True,
                "signed_live_end": True,
                "signed_video_delete": True,
                "forwarded_actual_video_abuse": True,
                "operator_media_block_restart": True,
                "paired_disk_expiry_and_cleanup_debt": debt_drill,
                "fresh_root_recovery": recovery,
            }
            (evidence / "result.json").write_text(json.dumps(result, indent=2))
            print(f"Protected remote-cache authority evidence: {evidence}")
    finally:
        if consumer._dir is not None:
            log = Path(consumer._dir.name) / "server.log"
            if log.exists():
                (evidence / "server.log").write_text(log.read_text())
            print(
                f"Protected remote-cache evidence: {evidence}; consumer log present: {log.exists()}"
            )
        try:
            if domain_block is not None:
                admin.delete(f"/api/v1/admin/domain_blocks/{domain_block}")
            for abuse_id in owned_abuse_ids:
                pt.delete(f"/api/v1/abuses/{abuse_id}")
            if suspended_channel is not None:
                admin.post(f"/api/v1/admin/accounts/{suspended_channel}/unsuspend")
            if vod is not None and not deleted:
                pt.delete_video(vod["id"])
            if live is not None:
                pt.delete_video(live["id"])
        finally:
            if consumer._dir is not None:
                log = Path(consumer._dir.name) / "server.log"
                if log.exists():
                    (evidence / "server.log").write_text(log.read_text())
            consumer.stop()
            if source is not consumer:
                source.stop()
            if restored is not consumer:
                restored.stop()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
