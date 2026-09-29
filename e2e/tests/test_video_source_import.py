"""Real PeerTube files/AP Video -> private native review -> new publication.

All accounts, channels and source films here are explicitly disposable local
fixtures. Importing never borrows the remote Video's identity or publishes on GET.
"""
import re
import tempfile
import tomllib
from pathlib import Path
from urllib.parse import urljoin

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.steps import step, wait_for


def _csrf(html):
    found = re.search(r'name="csrf" value="([^"]+)"', html)
    assert found, "ordinary HTML has no CSRF token"
    return found[1]


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="both", peer="peertube")
@pytest.mark.parametrize("source_kind", ["file", "activitypub"])
def test_peertube_source_import_is_private_native_and_explicitly_publishable(
        plamenu_api, plamenu_user, db, source_kind):
    api = plamenu_api
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("sourceimport")[:30]
    channel = api.post("/api/v1/video_channels", username=name, display_name="Imported films")
    source = draft = posted = None
    subscribed = False
    try:
        with step("PeerTube authors a finite source with downloadable fragmented MP4"):
            with tempfile.TemporaryDirectory() as temp:
                movie = peertube.make_sample_mp4(Path(temp) / "source.mp4", seconds=6, size="640x480")
                source = pt.upload_video(movie, f"Source {name}", privacy=1)
            pt.wait_for_hls(source["uuid"])
            original_url = pt.ap_id(source["uuid"])
            obj = requests.get(original_url, headers={"Accept": "application/activity+json"},
                               verify=False, timeout=30).json()
            files = [f for link in obj["url"] for f in [link, *link.get("tag", [])]
                     if f.get("mediaType") == "video/mp4"]
            assert files
            # Same-height later HLS file wins, matching the native selector.
            file_url = max(reversed(files), key=lambda f: f.get("height", 0))["href"]
            original_bytes = requests.get(file_url, verify=False, timeout=30).content
            source_url = original_url if source_kind == "activitypub" else file_url

        with step("ordinary HTML validates consent and CSRF before queuing a private import"):
            login = browser.post(config.PLAMENU_URL + "/login",
                                 data={"email": plamenu_user.email, "password": plamenu_user.password},
                                 allow_redirects=False, timeout=30)
            assert login.status_code == 303
            form = browser.get(config.PLAMENU_URL + "/videos/import", timeout=30)
            assert form.status_code == 200 and form.headers["Cache-Control"] == "private, no-store"
            data = {"csrf": _csrf(form.text), "channel": channel["id"], "title": f"Imported {name}",
                    "description": f"Copied with permission from {original_url}", "language": "en",
                    "visibility": "public", "source_kind": source_kind, "source_url": source_url,
                    "category": "12", "licence": "7", "alt": "Moving coloured test pattern.",
                    "transcript": "A steady test tone."}
            refused = browser.post(config.PLAMENU_URL + "/web/videos/import", data=data,
                                   allow_redirects=False, timeout=30)
            assert refused.status_code == 422
            assert browser.post(config.PLAMENU_URL + "/web/videos/import",
                                data=data | {"csrf": "wrong", "confirm": "yes"},
                                allow_redirects=False, timeout=30).status_code == 403
            response = browser.post(config.PLAMENU_URL + "/web/videos/import",
                                    data=data | {"confirm": "yes"}, allow_redirects=False, timeout=30)
            assert response.status_code == 303, response.text[:500]
            draft = urljoin(config.PLAMENU_URL, response.headers["Location"])
            media_id = draft.rsplit("/", 1)[1]
            phase_url = draft + "/status"
            def terminal_phase():
                value = browser.get(phase_url, timeout=30).json()
                return value if value.get("phase") in ("ready", "failed", "import-failed") else None
            phase = wait_for(terminal_phase,
                            desc="native import and actual transcode finish", timeout=180)
            assert phase["phase"] == "ready", phase
            assert requests.get(phase_url, verify=False, timeout=30).status_code == 404
            assert requests.get(draft + "/preview/video", verify=False, timeout=30).status_code == 404
            assert browser.get(config.PLAMENU_URL + f"/media/{media_id}.orig", timeout=30).status_code == 404
            assert db._value("SELECT status_id FROM media_attachments WHERE id=%s", int(media_id)) is None
            source_key = db._value("SELECT source_file FROM video_drafts WHERE media_id=%s", int(media_id))
            media_root = Path(tomllib.loads(Path(config.PLAMENU_CONFIG).read_text())["media_dir"])
            assert (media_root / source_key).read_bytes() == original_bytes
            assert db._value("SELECT source_url FROM video_source_imports WHERE media_id=%s", int(media_id)) == "imported"

        with step("private review preserves alternatives and requires a separate publication"):
            review = browser.get(draft + "/review", timeout=30)
            assert review.status_code == 200 and "Moving coloured test pattern." in review.text
            assert "A steady test tone." in review.text
            preview = browser.get(draft + "/preview/video", headers={"Range": "bytes=0-4095"}, timeout=30)
            assert preview.status_code == 206 and len(preview.content) == 4096
            caption = browser.post(config.PLAMENU_URL + f"/web/videos/drafts/{media_id}/captions",
                                   files=[("csrf", (None, _csrf(review.text))), ("language", (None, "en")),
                                          ("caption", ("captions.vtt", b"WEBVTT\n\n00:00:00.000 --> 00:00:06.000\n[Steady tone]\n", "text/vtt"))],
                                   allow_redirects=False, timeout=30)
            assert caption.status_code == 303
            handle = f"{name}@{config.PLAMENU_DOMAIN}"
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            response = browser.post(config.PLAMENU_URL + "/web/videos",
                                    files=[(k, (None, str(v))) for k, v in {
                                        "csrf": _csrf(review.text), "channel": channel["id"], "media_id": media_id,
                                        "title": f"Imported {name}", "description": data["description"], "language": "en",
                                        "visibility": "public", "op": "publish", "category": "12", "licence": "7"}.items()],
                                    allow_redirects=False, timeout=60)
            assert response.status_code == 303, response.text[:500]
            posted = response.headers["Location"].rsplit("/", 1)[1]
            film = api.get_status(posted)
            assert film["object_type"] == "Video"
            media = film["media_attachments"][0]
            assert media["description"] == data["alt"] and media["transcript"] == data["transcript"]
            assert media["video_metadata"]["category"]["id"] == 12 and media["video_metadata"]["licence"]["id"] == 7
            assert media["hls"]["p2p"]["trackers"] == [f"wss://{config.PLAMENU_DOMAIN}/tracker/socket"]
            assert api.http.get(media["url"], headers={"Range": "bytes=17-80"}, timeout=30).status_code == 206
            remote = wait_for(lambda: next((v for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100, sort="-publishedAt")["data"]
                                           if v.get("name") == f"Imported {name}"), None), desc="PeerTube receives imported native Video")
            assert remote["uuid"] != source["uuid"]
            remote = pt.video(remote["uuid"])
            assert remote["url"] != original_url
            assert remote["category"]["id"] == 12 and remote["licence"]["id"] == 7
            assert pt.get(f"/api/v1/videos/{remote['uuid']}/captions")["total"] == 1
    finally:
        if posted:
            api.delete_status(posted)
            if subscribed:
                wait_for(lambda: not any(v.get("name") == f"Imported {name}" for v in
                                        pt.get("/api/v1/users/me/subscriptions/videos", count=100, sort="-publishedAt")["data"]),
                         desc="PeerTube acknowledges imported native Video deletion")
        elif draft:
            page = browser.get(draft, timeout=30)
            if page.status_code == 200:
                browser.post(config.PLAMENU_URL + f"/web/videos/drafts/{draft.rsplit('/',1)[1]}",
                             data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"}, timeout=30)
        if subscribed:
            response = pt.http.delete(f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}", timeout=30)
            assert response.status_code in (204, 404), response.text
        if source:
            pt.delete_video(source["uuid"])
        api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=channel["username"])
