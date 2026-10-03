"""Real optional native copies, signed CacheFile/Undo and source-loss playback.

Only owned videos, source/mirror servers, databases and roots are mutated.
The existing PeerTube instance stays running and its acceptance policy is not
changed. Both positive and failure evidence are retained outside temp roots.
"""

import json
import os
import socket
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path
from urllib.parse import urljoin

import psycopg
import pytest
import requests
import tomllib
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import step, wait_for
from psycopg import sql
from tests.test_video_backup_restore import _digest, _manifest, _stop_process
from tests.test_video_offload import _freeze, _submit
from tests.test_video_remote_cache_authority import _retain_fixture_instance_identity


def _get(url, **kwargs):
    return requests.get(url, verify=False, timeout=30, **kwargs)


@pytest.mark.skipif(
    os.environ.get("AP09_MIRROR_VERIFY") != "1",
    reason="owned two-instance signed redundancy drill",
)
@pytest.mark.federation(direction="both", peer="peertube")
@pytest.mark.parametrize("product", ["native", "peertube"])
def test_native_mirror_copies_complete_graph_and_sends_real_cachefile_undo(
    product, monkeypatch
):
    assert peertube.reachable()
    for port in (8421, 8422):
        with socket.socket() as guard:
            assert guard.connect_ex(("127.0.0.1", port)) != 0
    label = unique("federatedmirror").replace("-", "_")
    parent = config.PLAMENU_DIR / "target/e2e/video-federated-mirrors"
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-{product}-", dir=parent))
    evidence.chmod(0o700)
    mirror = ephemeral.Instance(
        domain=ephemeral.DOMAIN,
        bind="0.0.0.0:8421",
        db_name=f"plamenu_ap09_{label}_mirror",
    )
    source = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}_source",
    )
    pt = peertube.root()
    remote = source_api = source_channel = None
    subscription = None
    source_stopped = False
    receipts = {"source_product": product}
    try:
        with step(
            "frozen native mirror with bounded quota, pacing and retained instance identity"
        ):
            mirror.start()
            candidate = _freeze(mirror, evidence, monkeypatch)
            _stop_process(mirror)
            with mirror._config_path.open("a") as output:
                output.write(
                    "\n[video_redundancy]\nquota_gib=1\nmax_mirror_mib=64\nbandwidth_mib_per_second=1\nretention_hours=1\n"
                )
            mirror._launch(120, append_log=True)
            _retain_fixture_instance_identity(mirror)
            mirror.apply_suite_settings()
            _, operator = mirror.admin("mirroroperator")
            movie = peertube.make_sample_mp4(
                evidence / "source.mp4", seconds=20, size="640x480"
            )
            title = f"Complete {product} mirror {label}"
            if product == "native":
                source.start()
                source.apply_suite_settings()
                user, source_api = source.admin("sourcecreator")
                source_channel = source_api.post(
                    "/api/v1/video_channels",
                    username=unique("mirrorfilms")[:30],
                    display_name="Mirror films",
                )
                subscription = f"{source_channel['username']}@{source.domain}"
                pt.post("/api/v1/users/me/subscriptions", uri=subscription)
                wait_for(
                    lambda: source.db.follower_count(source_channel["username"]) > 0,
                    desc="actual PeerTube subscribes to owned source",
                )
                media, browser = _submit(source, user, source_channel, movie, title)
                browser.close()
                wait_for(
                    lambda: source.db_value(
                        "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                        media,
                    ),
                    desc="native ladder ready",
                    timeout=240,
                )
                source_api.put(
                    f"/api/v1/videos/{media}/distribution",
                    download_enabled="true",
                    embed_enabled="true",
                    mirror_allowed="true",
                )
                original = source_api.post(
                    "/api/v1/videos",
                    channel_id=source_channel["id"],
                    media_id=str(media),
                    title=title,
                    visibility="public",
                )
                source_api.put(
                    f"/api/v1/admin/video_mirror_approvals/{mirror.domain}",
                    allow_source="false",
                    allow_mirror="true",
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
                            if v["name"] == title
                        ),
                        None,
                    ),
                    desc="actual PeerTube stores source video",
                )
                uri = original["uri"]
                consent = ""
                domain = source.domain
            else:
                remote = pt.upload_video(movie, title)
                pt.wait_for_hls(remote["uuid"])

                def complete_publication():
                    # A first HLS rung is playable but not yet a stable full
                    # graph. Keep real canonical Update invalidation intact.
                    for job_type in ("video-transcoding", "transcoding-job-builder"):
                        for state in (
                            "active",
                            "waiting",
                            "waiting-children",
                            "prioritized",
                            "delayed",
                        ):
                            jobs = pt.get(
                                f"/api/v1/jobs/{state}", jobType=job_type, count=100
                            )
                            assert jobs["total"] <= 100
                            if any(
                                job["data"].get("videoUUID") == remote["uuid"]
                                for job in jobs["data"]
                            ):
                                return False
                    return True

                wait_for(
                    complete_publication,
                    desc="owned PeerTube complete graph finishes before immutable copy",
                    timeout=240,
                    interval=3,
                )
                uri = pt.ap_id(remote["uuid"])
                consent = "The test creator explicitly permits this owned video to be mirrored."
                domain = config.PEERTUBE_DOMAIN
            imported = operator.resolve_status(uri)
            operator.put(
                f"/api/v1/admin/video_mirror_approvals/{domain}",
                allow_source="true",
                allow_mirror="false",
            )
            mirror_media = imported["media_attachments"][0]["id"]
            if product == "peertube":
                refused = operator.http.post(
                    mirror.url + "/api/v1/admin/video_mirrors",
                    json={"status_id": int(imported["id"])},
                    timeout=30,
                )
                assert refused.status_code == 403, (
                    "downloadEnabled is not creator replication consent"
                )
            receipts.update(
                candidate_sha256=candidate, source_uri=uri, mirror_media=mirror_media
            )

        with step(
            "durable complete graph and real source receipt before public serving"
        ):
            copy_started = time.monotonic()
            requested = operator.post(
                "/api/v1/admin/video_mirrors",
                status_id=imported["id"],
                consent_attestation=consent,
            )
            mirror_id = requested["id"]
            ready = wait_for(
                lambda: (
                    value
                    if (
                        value := operator.get(
                            f"/api/v1/admin/video_mirrors/{mirror_id}"
                        )
                    )["state"]
                    == "ready"
                    else None
                ),
                desc="leased paced native copier finishes",
                timeout=180,
            )
            assets = mirror.db_value(
                "SELECT jsonb_agg(to_jsonb(a) ORDER BY public_name) FROM video_mirror_assets a WHERE mirror_id=%s",
                int(mirror_id),
            )
            assert assets and len(assets) >= 3
            assert ready["reserved_bytes"] == sum(asset["bytes"] for asset in assets)
            copy_seconds = time.monotonic() - copy_started
            assert copy_seconds >= ready["reserved_bytes"] / (1024 * 1024) * 0.95, (
                "paced copy cannot exceed its configured aggregate byte rate"
            )
            receipts.update(
                ready=ready,
                asset_count=len(assets),
                copy_seconds=copy_seconds,
                acknowledged_assets=[
                    {
                        key: asset[key]
                        for key in ("source_url", "public_name", "bytes", "sha256")
                    }
                    for asset in assets
                ],
            )
            (evidence / "copy-ready.json").write_text(json.dumps(receipts, indent=2))
            for asset in assets:
                assert (
                    _get(mirror.url + "/media/" + asset["file_name"]).status_code == 404
                )
            offer_uri = f"{mirror.url}/cachefiles/{mirror_id}"
            # CacheFile is an ActivityPub document under authorized_fetch,
            # unlike public media. Exercise a real signed peer fetch.
            offer = operator.ap_get(f"/cachefiles/{mirror_id}")
            assert offer["object"] == uri
            if product == "native":
                wait_for(
                    lambda: source.db_value(
                        "SELECT EXISTS(SELECT 1 FROM remote_video_mirrors WHERE uri=%s AND withdrawn_at IS NULL)",
                        offer_uri,
                    ),
                    desc="source native inbox authenticates actual CacheFile",
                )
            else:

                def accepted():
                    video = pt.video(remote["uuid"])
                    return any(
                        offer["url"]["href"]
                        in json.dumps(playlist.get("redundancies", []))
                        for playlist in video["streamingPlaylists"]
                    )

                wait_for(
                    accepted,
                    desc="actual PeerTube accepts signed CacheFile and advertises redundancy",
                    timeout=90,
                )
            base = offer["url"]["href"]
            master = urljoin(base, "master.m3u8")
            body = _get(master)
            assert body.status_code == 200
            child = urljoin(
                master,
                next(
                    line
                    for line in body.text.splitlines()
                    if line and not line.startswith("#")
                ),
            )
            assert _get(child).status_code == 200
            # Decode every copied media through the public mirror playlist,
            # not private files, and retain the output/actual exit status.
            with (evidence / "mirror-full-decode.log").open("w") as output:
                decoded = subprocess.run(
                    [
                        "ffmpeg",
                        "-v",
                        "error",
                        "-xerror",
                        "-tls_verify",
                        "0",
                        "-i",
                        master,
                        "-map",
                        "0:v:0",
                        "-map",
                        "0:a:0",
                        "-f",
                        "null",
                        "-",
                    ],
                    stdout=output,
                    stderr=subprocess.STDOUT,
                    timeout=90,
                    check=False,
                )
            assert decoded.returncode == 0
            receipts["ready"] = ready
            receipts["asset_count"] = len(assets)

        with step(
            "paired backup restores mirror identity and exact assets into fresh DB and storage"
        ):
            _stop_process(mirror)
            original_config = mirror._config_path.read_text()
            settings = tomllib.loads(original_config)
            original_root = Path(settings["media_dir"])
            stored_files = _manifest(original_root)
            assert stored_files
            backup = evidence / "recovery-set"
            escrow = evidence / "configuration-escrow"
            backup.mkdir(mode=0o700)
            escrow.mkdir(mode=0o700)
            with (backup / "database.dump").open("wb") as output:
                subprocess.run(
                    [
                        "pg_dump",
                        "--format=custom",
                        "--no-owner",
                        "--dbname",
                        mirror.dsn,
                    ],
                    stdout=output,
                    check=True,
                    timeout=120,
                )
            with tarfile.open(backup / "media.tar.gz", "w:gz") as archive:
                archive.add(original_root, arcname="media")
            (escrow / "plamenu.toml").write_text(original_config)
            (escrow / "plamenu.toml").chmod(0o600)
            (backup / "manifest.json").write_text(json.dumps(stored_files, indent=2))
            for path in backup.iterdir():
                path.chmod(0o600)
            restored = ephemeral.Instance(
                domain=mirror.domain,
                bind=mirror.bind,
                db_name=mirror.db_name + "_recovered",
            )
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
                prefix="ap09-mirror-recovered-",
                dir=config.PLAMENU_DIR / "target/e2e/instances",
            )
            restored_root = Path(restored._dir.name)
            with tarfile.open(backup / "media.tar.gz", "r:gz") as archive:
                archive.extractall(restored_root, filter="data")
            assert _manifest(restored_root / "media") == stored_files
            recovered_config = original_config.replace(
                mirror.dsn, restored.dsn
            ).replace(str(original_root), str(restored_root / "media"))
            restored._config_path.write_text(recovered_config)
            restored._config_path.chmod(0o600)
            wrong = restored_root / "wrong-secret.toml"
            wrong.write_text(
                recovered_config.replace(
                    settings["encryption_secret"], "incorrect-recovery-secret"
                )
            )
            wrong.chmod(0o600)
            negative = subprocess.run(
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
            assert negative.returncode != 0
            assert "plaintext audit clean" in restored.cli(
                "federation", "keys", "audit"
            )
            assert (
                restored.db_value(
                    "SELECT jsonb_agg(to_jsonb(a) ORDER BY public_name) FROM video_mirror_assets a WHERE mirror_id=%s",
                    int(mirror_id),
                )
                == assets
            )
            (evidence / "mirror-before-restore.log").write_text(
                (Path(mirror._dir.name) / "server.log").read_text()
            )
            mirror.stop()
            assert not original_root.exists(), (
                "playback must not depend on original media root"
            )
            mirror = restored
            mirror._launch(120, append_log=False)
            assert (
                operator.get(f"/api/v1/admin/video_mirrors/{mirror_id}")[
                    "reserved_bytes"
                ]
                == ready["reserved_bytes"]
            )
            assert operator.ap_get(f"/cachefiles/{mirror_id}") == offer
            receipts["paired_fresh_restore"] = {
                "files": len(stored_files),
                "wrong_secret_refused": True,
                "recovery_sha256": {
                    path.name: _digest(path) for path in backup.iterdir()
                },
            }

        with step(
            "cold native JS/non-JS decoder survives source loss using restored mirror"
        ):
            if product == "native":
                _stop_process(source)
                source_stopped = True
            mirror.restart_process_after_crash()
            browser_manifest = evidence / "playback.json"
            browser_manifest.write_text(
                json.dumps(
                    [
                        {
                            "native_url": f"{mirror.url}/web/statuses/{imported['id']}",
                            "peertube_url": f"{config.PEERTUBE_URL}/w/{remote['uuid']}",
                            "skip_peer": product == "native",
                        }
                    ]
                )
            )
            with (evidence / "browser.log").open("w") as output:
                browser = subprocess.run(
                    [
                        "node",
                        str(
                            config.PLAMENU_DIR
                            / "accessibility/verify-mirror-playback.mjs"
                        ),
                        str(browser_manifest),
                        str(evidence),
                    ],
                    stdout=output,
                    stderr=subprocess.STDOUT,
                    timeout=120,
                    env={
                        **os.environ,
                        "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                        "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
                    },
                    check=False,
                )
            assert browser.returncode == 0, (
                "inspect actual mirror decoders and screenshots"
            )
            if product == "native":
                source._launch(120, append_log=True)
                source_stopped = False
            receipts["playback"] = json.loads(
                (evidence / "mirror-playback.json").read_text()
            )

        with step("signed Undo, immediate warm denial and acknowledged storage refund"):
            operator.delete(f"/api/v1/admin/video_mirrors/{mirror_id}")
            assert _get(master).status_code == 404
            with pytest.raises(ApiError, match="404"):
                operator.ap_get(f"/cachefiles/{mirror_id}")
            if product == "native":
                wait_for(
                    lambda: source.db_value(
                        "SELECT EXISTS(SELECT 1 FROM remote_video_mirrors WHERE uri=%s AND withdrawn_at IS NOT NULL)",
                        offer_uri,
                    ),
                    desc="native source authenticates actual Undo",
                )
            else:
                wait_for(
                    lambda: (
                        not any(
                            base in json.dumps(p.get("redundancies", []))
                            for p in pt.video(remote["uuid"])["streamingPlaylists"]
                        )
                    ),
                    desc="actual PeerTube removes authenticated mirror Undo",
                )
            wait_for(
                lambda: (
                    operator.get("/api/v1/admin/video_mirrors")["charged_bytes"] == 0
                ),
                desc="all mirror filesystem deletes acknowledged",
            )
            receipts["signed_create_and_undo_received"] = True
            (evidence / "result.json").write_text(json.dumps(receipts, indent=2))
            print(f"Protected real mirror evidence: {evidence}")
    finally:
        try:
            if source_stopped:
                source._launch(120, append_log=True)
            if (
                source_channel is not None
                and source._proc is not None
                and source._proc.poll() is None
            ):
                source_api.post(
                    f"/api/v1/video_channels/{source_channel['id']}",
                    operation="delete",
                    confirm=source_channel["username"],
                )
            if subscription:
                pt.delete(f"/api/v1/users/me/subscriptions/{subscription}")
            if product == "peertube" and remote is not None:
                pt.delete_video(remote["id"])
        finally:
            for instance, name in ((mirror, "mirror"), (source, "source")):
                if instance._dir is not None:
                    log = Path(instance._dir.name) / "server.log"
                    if log.exists():
                        (evidence / f"{name}.log").write_text(log.read_text())
                instance.stop()
            for path in evidence.rglob("*"):
                if path.is_file():
                    path.chmod(0o700 if path.name == "candidate-plamenu" else 0o600)
