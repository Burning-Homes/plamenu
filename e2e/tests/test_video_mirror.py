"""Optional paired disks: real native files, failover, authority and recovery.

Only a uniquely named disposable database and the guarded plamenu2 port are
modified. No standing media, account or backup is used as a fault target.
"""

import hashlib
import json
import socket
import tempfile
from pathlib import Path
from urllib.parse import urljoin, urlsplit

import pytest
import requests
from plamenu_e2e import config, ephemeral, peertube, unique
from plamenu_e2e.steps import step, wait_for
from test_video_backup_restore import _csrf, _playback_assets, _stop_process


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="PeerTube consumes native files from a surviving optional replica",
)
def test_native_paired_disks_seed_failover_revoke_and_resume():
    with socket.socket() as guard:
        assert guard.connect_ex(("127.0.0.1", 8422)) != 0, (
            "disposable peer port is occupied"
        )
    label = unique("mirror").replace("-", "_")
    peer = ephemeral.Instance(
        domain=config.PLAMENU2_DOMAIN,
        bind="0.0.0.0:8422",
        db_name=f"plamenu_ap09_{label}",
    )
    browser = requests.Session()
    browser.verify = False
    pt = peertube.root()
    handle = None
    subscribed = False
    evidence_root = config.PLAMENU_DIR / "target/e2e/video-mirror"
    evidence_root.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix=f"{label}-", dir=evidence_root))
    evidence.chmod(0o700)
    try:
        with step("seed a pre-existing real film when opting into two native disks"):
            peer.start(suite_settings=True)
            user, api = peer.user_api("mirrorauthor")
            channel = api.post(
                "/api/v1/video_channels",
                username=unique("mirrorfilms")[:30],
                display_name="Mirrored films",
            )
            handle = f"{channel['username']}@{peer.domain}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            wait_for(
                lambda: peer.db.follower_count(channel["username"]) > 0,
                desc="actual PeerTube subscribes to mirrored creator",
            )
            login = browser.post(
                f"{peer.url}/login",
                data={"email": user.email, "password": user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303
            page = browser.get(f"{peer.url}/videos", timeout=30)
            csrf = _csrf(page.text)
            movie = peertube.make_sample_mp4(
                evidence / "sample.mp4", seconds=8, size="640x480"
            )
            fields = {
                "csrf": csrf,
                "channel": channel["id"],
                "title": f"Mirror {label}",
                "description": "A moving pattern and steady tone.",
                "visibility": "public",
                "language": "en",
                "alt": "A moving test pattern.",
                "op": "review",
            }
            with movie.open("rb") as stream:
                uploaded = browser.post(
                    f"{peer.url}/web/videos",
                    files=[
                        *((k, (None, str(v))) for k, v in fields.items()),
                        ("file", ("sample.mp4", stream, "video/mp4")),
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
                    timeout=90,
                )
            assert uploaded.status_code == 303, uploaded.text[:500]
            draft = urljoin(peer.url, uploaded.headers["Location"])
            media_id = urlsplit(draft).path.rsplit("/", 1)[1]
            wait_for(
                lambda: peer.db_value(
                    "SELECT state='ready' FROM video_drafts WHERE media_id=%s",
                    int(media_id),
                ),
                desc="actual native ladder finishes",
                timeout=180,
            )
            film = api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=media_id,
                title=fields["title"],
                visibility="public",
            )
            media = film["media_attachments"][0]
            original_assets = _playback_assets(browser, media)
            remote = wait_for(
                lambda: next(
                    (
                        v
                        for v in pt.get(
                            "/api/v1/users/me/subscriptions/videos",
                            count=100,
                            sort="-publishedAt",
                        )["data"]
                        if v["name"] == fields["title"]
                    ),
                    None,
                ),
                desc="PeerTube imports the real film",
            )
            primary = Path(peer._dir.name) / "media"
            replica = Path(peer._dir.name) / "replica"
            replica.mkdir()
            _stop_process(peer)
            with peer._config_path.open("a") as config_file:
                config_file.write(
                    f'\n[media_mirror]\ndirectory = "{replica}"\nbandwidth_mib_per_second = 32\nmin_free_gib = 0\n'
                )
            peer._launch(120, append_log=True)
            keys = [
                p.name
                for p in primary.iterdir()
                if p.is_file() and not p.name.startswith(".plamenu-")
            ]
            digests = {
                key: hashlib.sha256((primary / key).read_bytes()).hexdigest()
                for key in keys
            }
            # The root also contains the real subscriber's cached avatar.
            # Publication deletion must preserve unrelated referenced media.
            film_keys = [
                row[0]
                for row in peer.db.conn.execute(
                    "SELECT file_name FROM video_storage_reservations WHERE media_id=%s",
                    (int(media_id),),
                ).fetchall()
            ]
            assert film_keys and set(film_keys).issubset(keys)
            assert keys
            for key in keys:
                assert (
                    hashlib.sha256((replica / key).read_bytes()).hexdigest()
                    == digests[key]
                )
            assert _playback_assets(browser, media) == original_assets

        with step(
            "fail over exact MP4 and HLS bytes without bypassing publication authority"
        ):
            for key in keys:
                (
                    primary / key
                ).unlink()  # Exact disposable fixture keys, never standing media.
            assert _playback_assets(browser, media) == original_assets
            file_url = pt.video(remote["uuid"])["files"][0]["fileUrl"]
            ranged = requests.get(
                file_url, headers={"Range": "bytes=0-31"}, verify=False, timeout=30
            )
            assert ranged.status_code == 206 and len(ranged.content) == 32
            actor = peer.db_value(
                "SELECT id FROM accounts WHERE username=%s AND domain IS NULL",
                user.username,
            )
            peer.db.conn.execute(
                "UPDATE accounts SET suspended_at=now() WHERE id=%s", (actor,)
            )
            peer.db.conn.commit()
            for url in original_assets:
                for session in (browser, requests.Session()):
                    refused = session.get(url, verify=False, timeout=30)
                    assert refused.status_code == 404, url
                    assert "no-store" in refused.headers.get("Cache-Control", "")
            peer.db.conn.execute(
                "UPDATE accounts SET suspended_at=NULL WHERE id=%s", (actor,)
            )
            peer.db.conn.commit()
            assert _playback_assets(browser, media) == original_assets

        with step(
            "mirrored resumable upload acknowledges both prefixes and survives a process crash"
        ):
            contents = movie.read_bytes()
            edge = min(65536, len(contents))
            fingerprint = hashlib.sha256(contents[:edge] + contents[-edge:]).hexdigest()
            page = browser.get(f"{peer.url}/videos", timeout=30)
            fields.update(
                csrf=_csrf(page.text),
                title="Mirrored unfinished upload",
                source_size=len(contents),
                source_fingerprint=fingerprint,
            )
            initiated = browser.post(
                f"{peer.url}/web/videos/resumable",
                files=[(k, (None, str(v))) for k, v in fields.items()],
                timeout=30,
            )
            initiated.raise_for_status()
            upload_id = initiated.json()["id"]
            source_key = peer.db_value(
                "SELECT source_file FROM video_drafts WHERE media_id=%s", int(upload_id)
            )
            offset = min(65536, len(contents) // 2)
            chunk = browser.patch(
                f"{peer.url}/web/videos/drafts/{upload_id}/source",
                data=contents[:offset],
                headers={
                    "Content-Type": "application/octet-stream",
                    "X-CSRF-Token": fields["csrf"],
                    "X-Upload-Offset": "0",
                },
                timeout=30,
            )
            assert chunk.status_code == 200, chunk.text[:500]
            assert (primary / source_key).read_bytes() == contents[:offset]
            assert (replica / source_key).read_bytes() == contents[:offset]
            peer.restart_process_after_crash()
            assert (
                peer.db_value(
                    "SELECT source_bytes FROM video_drafts WHERE media_id=%s",
                    int(upload_id),
                )
                == offset
            )
            assert (replica / source_key).read_bytes() == contents[:offset]

        with step(
            "deletion revokes both offers and refunds only after both physical removals"
        ):
            response = api.delete(f"/api/v1/statuses/{film['id']}")
            assert response is not None
            wait_for(
                lambda: all(
                    not (primary / key).exists() and not (replica / key).exists()
                    for key in film_keys
                ),
                desc="both publication copies physically removed",
                timeout=90,
            )
            for key in set(keys) - set(film_keys):
                assert (replica / key).exists(), (
                    "unrelated subscriber media must survive"
                )
            wait_for(
                lambda: (
                    peer.db_value(
                        "SELECT count(*) FROM video_storage_reservations WHERE media_id=%s",
                        int(media_id),
                    )
                    == 0
                ),
                desc="deleted publication storage charges are acknowledged",
                timeout=30,
            )
            assert browser.get(media["url"], timeout=30).status_code == 404
            (evidence / "manifest.json").write_text(
                json.dumps(
                    {
                        "keys": digests,
                        "assets": original_assets,
                        "range_bytes": 32,
                        "resumed_offset": offset,
                        "candidate_sha256": hashlib.sha256(
                            ephemeral.binary_path().read_bytes()
                        ).hexdigest(),
                    },
                    indent=2,
                )
            )
    finally:
        if subscribed:
            pt._request("DELETE", f"/api/v1/users/me/subscriptions/{handle}")
        peer.stop()
