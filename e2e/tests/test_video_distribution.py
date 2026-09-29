"""Creator download choices in real PeerTube Creates/Updates; local framing authority."""
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import apsign, config, peertube, unique
from plamenu_e2e.steps import step, wait_for
from test_video_edit import csrf, prepare


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="outbound", peer="peertube",
                       one_way_reason="Native creator distribution choices propagate as standard downloadEnabled")
def test_native_distribution_choices_survive_create_update_and_delete(plamenu_api, plamenu_user):
    api, pt = plamenu_api, peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("sharing")[:30]
    channel = api.post("/api/v1/video_channels", username=name, display_name="Sharing films")
    draft = posted = None
    subscribed = False
    try:
        login = browser.post(config.PLAMENU_URL + "/login", data={"email": plamenu_user.email,
                             "password": plamenu_user.password}, allow_redirects=False, timeout=30)
        assert login.status_code == 303
        with tempfile.TemporaryDirectory() as temporary:
            movie = peertube.make_sample_mp4(Path(temporary) / "film.mp4", seconds=8, size="640x360")
            draft = prepare(browser, channel, movie, f"Sharing {name}", "Steady tone")
        media = draft.rsplit("/", 1)[1]
        settings_url = f"{config.PLAMENU_URL}/videos/{media}/sharing"

        def choices(download, embed):
            page = browser.get(settings_url, timeout=30)
            assert page.status_code == 200 and page.headers["Cache-Control"] == "private, no-store"
            fields = {"csrf": csrf(page.text)}
            if download:
                fields["download_enabled"] = "yes"
            if embed:
                fields["embed_enabled"] = "yes"
            saved = browser.post(f"{config.PLAMENU_URL}/web/videos/{media}/sharing", data=fields,
                                 allow_redirects=False, timeout=30)
            assert saved.status_code == 303, saved.text[:500]
            assert api.get(f"/api/v1/videos/{media}/distribution") == {
                "download_enabled": download, "embed_enabled": embed}

        with step("native private review chooses distribution before the first signed Create"):
            choices(False, False)
            assert requests.get(f"{config.PLAMENU_URL}/media/{media}/download", verify=False, timeout=30).status_code == 404
            pt.post("/api/v1/users/me/subscriptions", uri=f"{name}@{config.PLAMENU_DOMAIN}")
            subscribed = True
            film = api.post("/api/v1/videos", channel_id=channel["id"], media_id=media,
                            title=f"Sharing {name}", description="Sharing controls preserve ordinary playback",
                            visibility="public", language="en")
            posted = film["id"]
            remote = wait_for(lambda: next((v for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100, sort="-publishedAt")["data"]
                                           if v.get("name") == f"Sharing {name}"), None), desc="PeerTube receives download-disabled native Video")
            detail = pt.video(remote["uuid"])
            assert detail["downloadEnabled"] is False
            assert len(pt.get(f"/api/v1/videos/{remote['uuid']}/captions")["data"]) == 1
            original = requests.get(film["media_attachments"][0]["url"], verify=False, timeout=30)
            assert original.status_code == 200
            assert apsign.signed_ap_get(film["uri"]).json()["downloadEnabled"] is False
            assert browser.get(f"{config.PLAMENU_URL}/web/statuses/{posted}/embed?p2p=0", timeout=30).status_code == 404

        with step("current HTML changes deliver a stable-identity PeerTube Update and ranged attachment"):
            choices(True, True)
            enabled = wait_for(lambda: (value if (value := pt.video(remote["uuid"]))["downloadEnabled"] else None),
                               desc="PeerTube acknowledges download-enabled Update")
            assert enabled["uuid"] == remote["uuid"] and enabled["url"] == detail["url"]
            assert enabled["streamingPlaylists"] == detail["streamingPlaylists"]
            download_url = api.get_status(posted)["media_attachments"][0]["video_distribution"]["download_url"]
            downloaded = requests.get(download_url, verify=False, timeout=30)
            assert downloaded.status_code == 200 and downloaded.content == original.content
            assert downloaded.headers["Content-Disposition"] == f'attachment; filename="video-{media}.mp4"'
            assert downloaded.headers["Cache-Control"] == "private, no-store"
            ranged = requests.get(download_url, headers={"Range": "bytes=17-80"}, verify=False, timeout=30)
            assert ranged.status_code == 206 and ranged.content == original.content[17:81]
            embed_url = f"{config.PLAMENU_URL}/web/statuses/{posted}/embed?p2p=0"
            assert requests.get(embed_url, verify=False, timeout=30).status_code == 200
            choices(False, False)
            wait_for(lambda: pt.video(remote["uuid"])["downloadEnabled"] is False,
                     desc="PeerTube acknowledges disabled download Update")
            assert browser.get(embed_url, timeout=30).status_code == 404
            assert requests.get(download_url, verify=False, timeout=30).status_code == 404
            assert requests.get(film["media_attachments"][0]["url"], verify=False, timeout=30).content == original.content

        with step("signed Delete is acknowledged and stale distribution paths fail closed"):
            api.delete_status(posted)
            posted = None
            wait_for(lambda: pt.http.get(f"{pt.base_url}/api/v1/videos/{remote['uuid']}", timeout=30).status_code == 404,
                     desc="PeerTube acknowledges sharing Video Delete")
            assert requests.get(download_url, verify=False, timeout=30).status_code == 404
            assert requests.get(embed_url, verify=False, timeout=30).status_code == 404
    finally:
        if posted:
            api.delete_status(posted)
        if draft:
            page = browser.get(draft, timeout=30)
            if page.status_code == 200:
                browser.post(f"{config.PLAMENU_URL}/web/videos/drafts/{draft.rsplit('/',1)[1]}",
                             data={"csrf": csrf(page.text), "op": "cancel", "confirm": "yes"}, timeout=30)
        if subscribed:
            pt.http.delete(f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}", timeout=30)
        api.post(f"/api/v1/video_channels/{channel['id']}", operation="delete", confirm=channel["username"])
