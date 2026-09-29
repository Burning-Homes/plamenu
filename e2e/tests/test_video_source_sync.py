"""Actual PeerTube channel/Playlist discovery -> durable private native copies.

Only dedicated local fixtures are changed. A scheduled source never borrows
the creator's identity or publishes; publication remains a separate HTML POST.
"""
import os
import subprocess
import tempfile
import tomllib
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.steps import step, wait_for
from test_video_source_import import _csrf


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="both", peer="peertube")
@pytest.mark.parametrize("kind", ["channel", "playlist"])
def test_scheduled_peertube_source_copies_are_private_deduplicated_and_reviewable(
        plamenu_api, plamenu_user, db, kind):
    api, pt = plamenu_api, peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("syncsource")[:30]
    owner = int(api.get("/api/v1/accounts/verify_credentials")["id"])
    channel = api.post("/api/v1/video_channels", username=name, display_name="Scheduled copies")
    remote_channel = playlist = sync = published = None
    films, copies = [], []
    subscribed = False
    try:
        with step("create a dedicated PeerTube public channel or Playlist"):
            remote_channel = pt.post("/api/v1/video-channels", name=name, displayName="Sync source films")["videoChannel"]
            if kind == "playlist":
                playlist = pt.post("/api/v1/video-playlists", displayName="Sync source playlist", privacy=1,
                                   videoChannelId=remote_channel["id"])["videoPlaylist"]
            source_url = (f"{pt.base_url}/video-playlists/{playlist['uuid']}" if playlist else
                          f"{pt.base_url}/video-channels/{name}")

        def author(index):
            with tempfile.TemporaryDirectory() as temporary:
                movie = peertube.make_sample_mp4(Path(temporary) / "source.mp4", seconds=5, size="640x360")
                film = pt.upload_video(movie, f"Sync source {index} {name}", privacy=1,
                                       channel_id=remote_channel["id"])
            films.append(film)
            pt.wait_for_hls(film["uuid"])
            pt.put(f"/api/v1/videos/{film['uuid']}", category=12, licence=2, nsfw=False)
            if playlist:
                # Two clipped elements must become one complete private Video,
                # not two duplicates or a guessed source-level clip.
                for start in (0, 2):
                    pt.post(f"/api/v1/video-playlists/{playlist['id']}/videos", videoId=film["id"],
                            startTimestamp=start, stopTimestamp=start + 2)
            return film

        author(1)
        login = browser.post(config.PLAMENU_URL + "/login", data={"email": plamenu_user.email,
                             "password": plamenu_user.password}, allow_redirects=False, timeout=30)
        assert login.status_code == 303
        page = browser.get(config.PLAMENU_URL + "/videos/sources", timeout=30)
        csrf = _csrf(page.text)
        form = {"csrf": csrf, "channel": channel["id"], "source_url": source_url, "interval": "15"}
        with step("ordinary HTML requires explicit copying consent and private settings"):
            assert page.headers["Cache-Control"] == "private, no-store"
            assert page.headers["Referrer-Policy"] == "no-referrer"
            assert browser.post(config.PLAMENU_URL + "/web/videos/sources", data=form,
                                allow_redirects=False, timeout=30).status_code == 422
            assert browser.post(config.PLAMENU_URL + "/web/videos/sources", data=form | {"confirm": "yes"},
                                allow_redirects=False, timeout=30).status_code == 303
            sync = db._value("SELECT id FROM video_source_syncs WHERE account_id=%s AND source_url=%s",
                             owner, source_url)
            assert sync

        def imports():
            return [row[0] for row in db.conn.execute(
                "SELECT media_id FROM video_source_sync_items WHERE sync_id=%s ORDER BY media_id", (sync,)).fetchall()]

        def operation(action):
            response = browser.post(f"{config.PLAMENU_URL}/web/videos/sources/{sync}/{action}",
                                    data={"csrf": csrf, "confirm": "yes"}, allow_redirects=False, timeout=30)
            assert response.status_code == 303, response.text[:500]

        with step("scheduled discovery runs natively and never auto-publishes"):
            wait_for(lambda: len(imports()) == 1, desc="first scheduled private copy", timeout=120)
            first = imports()[0]
            copies.append(first)
            assert db._value("SELECT status_id FROM media_attachments WHERE id=%s", first) is None
            assert db._value("SELECT count(*) FROM statuses WHERE account_id=%s", owner) == 0
            public = requests.get(f"{config.PLAMENU_URL}/videos/drafts/{first}/status", verify=False, timeout=30)
            assert public.status_code == 404
            status = browser.get(config.PLAMENU_URL + "/videos/sources/state", timeout=30)
            assert source_url not in status.text and csrf not in status.text
            assert status.headers["Cache-Control"] == "private, no-store"
            operation("pause")
            author(2)
            assert db._value("SELECT enabled FROM video_source_syncs WHERE id=%s", sync) is False
            # Isolated fixture due-time adjustment avoids a fifteen-minute sleep;
            # the production worker and HTML pause/resume fencing are unchanged.
            db.conn.execute("UPDATE video_source_syncs SET next_check_at=now(),last_started_at=now()-interval '2 minutes' WHERE id=%s", (sync,))
            operation("resume")
            wait_for(lambda: len(imports()) == 2, desc="new source Video after resume", timeout=120)
            copies[:] = imports()

        with step("both private copies transcode and preserve exact source bytes and labels"):
            media_root = Path(tomllib.loads(Path(config.PLAMENU_CONFIG).read_text())["media_dir"])
            for media in copies:
                def terminal():
                    value = browser.get(f"{config.PLAMENU_URL}/videos/drafts/{media}/status", timeout=30).json()
                    return value if value.get("phase") in ("ready", "failed", "import-failed") else None
                assert wait_for(terminal, desc="scheduled native transcode", timeout=180)["phase"] == "ready"
                uri = db._value("SELECT source_uri FROM video_source_sync_items WHERE media_id=%s", media)
                obj = requests.get(uri, headers={"Accept": "application/activity+json"}, verify=False, timeout=30).json()
                files = [item for link in obj["url"] for item in [link, *link.get("tag", [])]
                         if item.get("mediaType") == "video/mp4"]
                file_url = max(reversed(files), key=lambda f: f.get("height", 0))["href"]
                key = db._value("SELECT source_file FROM video_drafts WHERE media_id=%s", media)
                assert (media_root / key).read_bytes() == requests.get(file_url, verify=False, timeout=30).content
                assert db._value("SELECT category FROM video_metadata WHERE media_id=%s", media) == 12
                # PeerTube advertises an identifier but normally no versioned
                # licence URI. Do not silently preselect a new 4.0 reuse grant.
                trusted = obj.get("licence", {}).get("url") == "https://creativecommons.org/licenses/by-sa/4.0/"
                assert db._value("SELECT licence FROM video_metadata WHERE media_id=%s", media) == (2 if trusted else None)
                assert browser.get(f"{config.PLAMENU_URL}/videos/drafts/{media}/preview/video",
                                   headers={"Range": "bytes=0-1023"}, timeout=30).status_code == 206
            db.conn.execute("UPDATE video_source_syncs SET last_started_at=now()-interval '2 minutes' WHERE id=%s", (sync,))
            operation("check")
            wait_for(lambda: db._value("SELECT last_result FROM video_source_syncs WHERE id=%s", sync) == "ok",
                     desc="repeat source check completes", timeout=120)
            assert len(imports()) == 2

        if kind == "playlist" and os.environ.get("AP09_BROWSER_SOURCE_SYNC") == "1":
            with step("all pinned desktop/mobile/no-JS projects author, pause, decode, cancel and remove a source"):
                root = config.PLAMENU_DIR
                report = "target/accessibility/ap09-source-sync-20260929/final/automated.json"
                subprocess.run(["python3", str(root / "scripts/run-accessibility-tests.py"),
                                "video-source-sync.spec.mjs", "--retries=0",
                                "--output=../target/accessibility/ap09-source-sync-20260929/final/test-results"],
                               cwd=root, env=os.environ | {"A11Y_SOURCE_SYNC_URL": source_url,
                                                         "A11Y_REPORT_PATH": report}, check=True, timeout=900)

        with step("removing discovery keeps copies; separate publication gets a new federated identity"):
            operation("remove")
            sync = None
            media = copies[0]
            review = browser.get(f"{config.PLAMENU_URL}/videos/drafts/{media}/review", timeout=30)
            assert review.status_code == 200
            pt.post("/api/v1/users/me/subscriptions", uri=f"{name}@{config.PLAMENU_DOMAIN}")
            subscribed = True
            title = f"Reviewed sync {name}"
            response = browser.post(config.PLAMENU_URL + "/web/videos", files=[(key, (None, str(value))) for key, value in {
                "csrf": _csrf(review.text), "media_id": media, "channel": channel["id"], "title": title,
                "description": "Reviewed private copy; source credited by its owner.", "language": "en",
                "visibility": "public", "op": "publish", "category": "12", "licence": "2"}.items()],
                allow_redirects=False, timeout=60)
            assert response.status_code == 303, response.text[:500]
            published = response.headers["Location"].rsplit("/", 1)[1]
            copies.remove(media)
            remote = wait_for(lambda: next((v for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100, sort="-publishedAt")["data"]
                                           if v.get("name") == title), None), desc="PeerTube accepts reviewed copied Video")
            assert remote["uuid"] not in [film["uuid"] for film in films]
            detail = pt.video(remote["uuid"])
            assert detail["category"]["id"] == 12 and detail["licence"]["id"] == 2
            api.delete_status(published)
            published = None
            wait_for(lambda: pt.http.get(f"{pt.base_url}/api/v1/videos/{remote['uuid']}", timeout=30).status_code == 404,
                     desc="PeerTube acknowledges copied Video signed Delete")
    finally:
        if sync:
            copies[:] = sorted(set(copies) | set(imports()))
            browser.post(f"{config.PLAMENU_URL}/web/videos/sources/{sync}/remove",
                         data={"csrf": csrf, "confirm": "yes"}, timeout=30)
        if published:
            api.delete_status(published)
        for media in copies:
            page = browser.get(f"{config.PLAMENU_URL}/videos/drafts/{media}", timeout=30)
            if page.status_code == 200:
                browser.post(f"{config.PLAMENU_URL}/web/videos/drafts/{media}",
                             data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"}, timeout=30)
        if subscribed:
            pt.http.delete(f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}", timeout=30)
        if playlist:
            pt.delete(f"/api/v1/video-playlists/{playlist['id']}")
        for film in films:
            pt.delete_video(film["uuid"])
        if remote_channel:
            pt.delete(f"/api/v1/video-channels/{name}")
        api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=channel["username"])
