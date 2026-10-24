"""Stopped-write recovery of a real native Video into fresh DB/media storage.

Only uniquely named disposable databases and the plamenu2.local fixture port
are used. Recovery archives and separately escrowed configuration are retained
under target/e2e with mode 0700; they contain private account data and keys.
"""

import hashlib
import json
import re
import socket
import subprocess
import tarfile
import tempfile
import tomllib
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for


def _csrf(page):
    match = re.search(r'name="csrf" value="([^"]+)"', page)
    assert match, "ordinary video form has no CSRF token"
    return match[1]


def _digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _manifest(root):
    return {
        str(p.relative_to(root)): _digest(p) for p in root.rglob("*") if p.is_file()
    }


def _stop_process(instance):
    # Quiesce just this fixture, preserving its source for an independent restore.
    assert instance._proc is not None
    instance._proc.terminate()
    instance._proc.wait(timeout=30)
    instance._log.close()


def _fetch(session, url):
    response = session.get(url, timeout=30)
    response.raise_for_status()
    return response


def _playback_assets(session, media):
    """Exact immutable MP4/poster/storyboard/HLS bytes and mutable captions."""
    urls = [
        media["url"],
        media["preview_url"],
        media["caption_url"],
        media["storyboard"]["url"],
        media["hls"]["master"],
        media["hls"]["p2p"]["sha256_url"],
    ]
    master = _fetch(session, media["hls"]["master"])
    variants = [
        urljoin(master.url, line)
        for line in master.text.splitlines()
        if line and not line.startswith("#")
    ]
    assert len(variants) >= 2
    urls.extend(variants)
    for variant in variants:
        response = _fetch(session, variant)
        urls.extend(
            urljoin(variant, line)
            for line in response.text.splitlines()
            if line and not line.startswith("#")
        )
    return {
        url: hashlib.sha256(_fetch(session, url).content).hexdigest()
        for url in set(urls)
    }


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="restored original keys sign a Video Update for a surviving PeerTube subscriber",
)
def test_native_video_restore_preserves_assets_offsets_preferences_and_peer_identity():
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "plamenu2 fixture port is already occupied"
        )
    label = unique("restore").replace("-", "_")
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
    artifacts = config.PLAMENU_DIR / "target/e2e/video-restore"
    artifacts.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=artifacts))
    evidence.chmod(0o700)
    backup, escrow = evidence / "recovery-set", evidence / "configuration-escrow"
    backup.mkdir(mode=0o700)
    escrow.mkdir(mode=0o700)
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    channel = published = playlist = None
    subscribed = False
    active = source
    try:
        with step(
            "prepare native renditions, captions, history and an unfinished upload"
        ):
            source.start(suite_settings=True)
            user, api = source.user_api("restoreauthor")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("restorefilms")[:30],
                display_name="Recovery films",
            )
            handle = f"{channel['username']}@{source.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: source.db.follower_count(channel["username"]) > 0,
                desc="surviving PeerTube subscribes before backup",
            )
            response = browser.post(
                f"{source.url}/login",
                data={"email": user.email, "password": user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert response.status_code == 303
            page = _fetch(browser, f"{source.url}/videos")
            csrf = _csrf(page.text)
            sample = peertube.make_sample_mp4(
                evidence / "source.mp4", seconds=8, size="640x480"
            )
            contents = sample.read_bytes()
            fields = {
                "csrf": csrf,
                "channel": channel["id"],
                "title": "Recovery native film",
                "description": "A moving pattern and steady tone.",
                "visibility": "public",
                "language": "en",
                "alt": "A moving test pattern.",
                "transcript": "A steady tone.",
                "op": "review",
            }
            uploaded = browser.post(
                f"{source.url}/web/videos",
                files=[(k, (None, str(v))) for k, v in fields.items()]
                + [
                    ("file", ("source.mp4", contents, "video/mp4")),
                    (
                        "captions",
                        (
                            "captions.vtt",
                            b"WEBVTT\n\n00:00:00.000 --> 00:00:08.000\n[Test tone]\n",
                            "text/vtt",
                        ),
                    ),
                ],
                allow_redirects=False,
                timeout=60,
            )
            assert uploaded.status_code == 303, uploaded.status_code
            draft = uploaded.headers["Location"]
            media_id = urlsplit(draft).path.rsplit("/", 1)[1]
            wait_for(
                lambda: source.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                    int(media_id),
                ),
                desc="actual native ladder and storyboard finish",
                timeout=180,
            )
            published = api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=media_id,
                title="Recovery native film",
                visibility="public",
            )
            published_source = source.db_value(
                "SELECT source_file FROM video_drafts WHERE media_id=%s", int(media_id)
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
                        if v["name"] == published["title"]
                    ),
                    None,
                ),
                desc="PeerTube receives original native film",
            )
            media = published["media_attachments"][0]
            assets = _playback_assets(browser, media)
            playlist = api.post(
                "/api/v1/video_playlists",
                channel_id=channel["id"],
                name="Private recovery clips",
                visibility="private",
            )
            api.post(
                f"/api/v1/video_playlists/{playlist['id']}",
                operation="add",
                revision=playlist["revision"],
                status_id=published["id"],
                start_seconds=1,
                stop_seconds=5,
            )
            playlist = api.get(f"/api/v1/video_playlists/{playlist['id']}")
            prefs = api._request(
                "PUT",
                "/api/v1/video_viewer_preferences",
                json={"history_enabled": True, "p2p_enabled": False},
            )
            api._request(
                "PUT", f"/api/v1/video_history/{media_id}", json={"position_seconds": 2}
            )
            history = api.get("/api/v1/video_history")
            edge = min(65536, len(contents))
            fingerprint = hashlib.sha256(contents[:edge] + contents[-edge:]).hexdigest()
            initiated = browser.post(
                f"{source.url}/web/videos/resumable",
                files=[
                    (k, (None, str(v)))
                    for k, v in {
                        **fields,
                        "title": "Upload surviving restore",
                        "source_size": len(contents),
                        "source_fingerprint": fingerprint,
                    }.items()
                ],
                timeout=30,
            )
            initiated.raise_for_status()
            upload_id = initiated.json()["id"]
            upload_source = source.db_value(
                "SELECT source_file FROM video_drafts WHERE media_id=%s", int(upload_id)
            )
            upload_url = f"{source.url}/web/videos/drafts/{upload_id}/source"
            offset = len(contents) // 2
            chunk = browser.patch(
                upload_url,
                headers={"x-csrf-token": csrf, "x-upload-offset": "0"},
                data=contents[:offset],
                timeout=30,
            )
            chunk.raise_for_status()
            assert chunk.json()["offset"] == offset

        with step(
            "stop writers and snapshot database, whole media tree and separately escrowed secret"
        ):
            _stop_process(source)
            postgres_major = int(source.db_value("SHOW server_version_num")) // 10000
            original_config = source._config_path.read_text()
            settings = tomllib.loads(original_config)
            original_media = Path(settings["media_dir"])
            files = _manifest(original_media)
            assert files
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
            with tarfile.open(backup / "media.tar.gz", "w:gz") as archive:
                archive.add(original_media, arcname="media")
            (escrow / "plamenu.toml").write_text(original_config)
            (escrow / "plamenu.toml").chmod(0o600)
            (evidence / "source-server.log").write_text(
                (Path(source._dir.name) / "server.log").read_text()
            )
            (evidence / "source-server.log").chmod(0o600)
            reservations = source.db_value(
                "SELECT jsonb_agg(to_jsonb(r) ORDER BY file_name) FROM video_storage_reservations r"
            )
            account_id = source.db_value(
                "SELECT id FROM accounts WHERE username=%s", user.username
            )
            (backup / "manifest.json").write_text(json.dumps(files, indent=2) + "\n")
            for path in backup.iterdir():
                path.chmod(0o600)
            source.stop()  # only the freshly created fixture source is removed
            assert not original_media.exists()

        with step(
            "restore into a fresh database and media directory without changing canonical domains"
        ):
            restored.create_database()
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
                prefix="plamenu-ap09-recovered-",
                dir=config.PLAMENU_DIR / "target/e2e/instances",
            )
            root = Path(restored._dir.name)
            with tarfile.open(backup / "media.tar.gz", "r:gz") as archive:
                archive.extractall(root, filter="data")
            assert _manifest(root / "media") == files
            recovered_config = original_config.replace(
                source.dsn, restored.dsn
            ).replace(str(original_media), str(root / "media"))
            assert (
                tomllib.loads(recovered_config)["encryption_secret"]
                == settings["encryption_secret"]
            )
            restored._config_path.write_text(recovered_config)
            restored._config_path.chmod(0o600)
            wrong = root / "wrong-secret.toml"
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
            assert negative.returncode != 0, (
                "wrong encryption root unexpectedly decrypts restored identities"
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

        with step(
            "old credentials, private settings, captions, renditions and offsets survive recovery"
        ):
            assert api.get("/api/v1/accounts/verify_credentials")["id"] == str(
                account_id
            )
            assert restored.login(user).get("/api/v1/accounts/verify_credentials")[
                "id"
            ] == str(account_id)
            current = api.get_status(published["id"])
            for field in ("id", "uri", "title", "content", "visibility", "created_at"):
                assert current[field] == published[field], field
            assert current["media_attachments"][0] == media
            assert _playback_assets(browser, media) == assets
            position = api.get("/api/v1/video_history")
            assert position == history
            current_prefs = api.get("/api/v1/video_viewer_preferences")
            assert current_prefs["history_enabled"] == prefs["history_enabled"]
            assert (
                current_prefs["p2p_enabled"] is False
                and current_prefs["p2p_configured"] is True
            )
            assert api.get(f"/api/v1/video_playlists/{playlist['id']}") == playlist
            partial = browser.get(
                media["url"], headers={"Range": "bytes=17-80"}, timeout=30
            )
            assert partial.status_code == 206 and len(partial.content) == 64
            assert partial.content == _fetch(browser, media["url"]).content[17:81]
            for key in (published_source, upload_source):
                assert (
                    browser.get(f"{source.url}/media/{key}", timeout=30).status_code
                    == 404
                )
                assert (
                    requests.get(
                        f"{source.url}/media/{key}", verify=False, timeout=30
                    ).status_code
                    == 404
                )
            settings_page = _fetch(browser, f"{source.url}/videos/settings")
            assert 'data-p2p-default="false"' in settings_page.text
            resumed = _fetch(browser, upload_url).json()
            assert (resumed["offset"], resumed["total"], resumed["fingerprint"]) == (
                offset,
                len(contents),
                fingerprint,
            )
            completion = browser.patch(
                upload_url,
                headers={"x-csrf-token": csrf, "x-upload-offset": str(offset)},
                data=contents[offset:],
                timeout=30,
            )
            completion.raise_for_status()
            assert completion.json()["offset"] == len(contents)
            finished = browser.post(
                f"{upload_url}/finish", headers={"x-csrf-token": csrf}, timeout=30
            )
            finished.raise_for_status()
            wait_for(
                lambda: restored.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                    int(upload_id),
                ),
                desc="restored unfinished upload processes using native worker",
                timeout=180,
            )
            page = _fetch(browser, f"{source.url}/videos/drafts/{upload_id}")
            cancelled = browser.post(
                f"{source.url}/web/videos/drafts/{upload_id}",
                data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"},
                allow_redirects=False,
                timeout=30,
            )
            assert cancelled.status_code == 303
            wait_for(
                lambda: restored.db_value(
                    "SELECT count(*)=0 FROM video_storage_reservations WHERE media_id=%s",
                    int(upload_id),
                ),
                desc="acknowledged post-restore cleanup refunds upload/output reservations",
            )

        with step(
            "surviving PeerTube accepts a signed Update from the restored original keys"
        ):
            api.put(
                f"/api/v1/statuses/{published['id']}",
                title="Recovery native film updated",
                status="A recovered moving pattern and steady tone.",
            )
            wait_for(
                lambda: (
                    pt.video(remote["uuid"])["name"] == "Recovery native film updated"
                ),
                desc="restored signer delivers Update without changing Video UUID",
            )
            record = {
                "candidate_sha256": _digest(ephemeral.binary_path()),
                "database_major": postgres_major,
                "recovery_files": {p.name: _digest(p) for p in backup.iterdir()},
                "asset_count": len(assets),
                "stored_file_count": len(files),
                "upload_offset": offset,
                "peer_video_uuid": remote["uuid"],
                "checks": "assets/ranges/preferences/history/playlist/offset/cleanup/keys/signed PeerTube Update",
            }
            (evidence / "result.json").write_text(json.dumps(record, indent=2) + "\n")
            (evidence / "result.json").chmod(0o600)
            print(f"Recovery evidence (private backup/escrow): {evidence}")
    finally:
        for name, instance in (("source", source), ("restored", restored)):
            if instance._dir is not None:
                log = Path(instance._dir.name) / "server.log"
                if log.exists():
                    (evidence / f"{name}-server.log").write_text(log.read_text())
                    (evidence / f"{name}-server.log").chmod(0o600)
        if active._proc is not None and active._proc.poll() is None:
            if published:
                active.login(user).delete_status(published["id"])
            if channel:
                active.login(user).post(
                    f"/api/v1/video_channels/{channel['id']}",
                    operation="delete",
                    confirm=channel["username"],
                )
        if subscribed:
            pt.delete(f"/api/v1/users/me/subscriptions/{handle}")
        source.stop()
        restored.stop()
        browser.close()
