"""Real native processing, report handling and post-transfer origin revocation."""

import tempfile
from pathlib import Path
from urllib.parse import urljoin

import pytest
import requests
from plamenu_e2e import config, peertube, plamenu, unique
from plamenu_e2e.steps import step, wait_for
from test_video_edit import csrf, prepare


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="Local moderation revokes the origin; final channel deletion federates signed retractions",
)
def test_current_owner_moderation_revokes_native_bytes_then_peer_receives_delete(
    plamenu_api, plamenu_user, plamenu_admin, cli
):
    api, pt = plamenu_api, peertube.root()
    _, admin = plamenu_admin
    owner = plamenu.User()
    cli.account_add(owner)
    owner_api = plamenu.login(owner)
    owner_id = owner_api.get("/api/v1/accounts/verify_credentials")["id"]
    name = unique("takedown")[:30]
    channel = api.post(
        "/api/v1/video_channels", username=name, display_name="Moderation films"
    )
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    browser = requests.Session()
    browser.verify = False
    draft = film = None
    subscribed = transferred = suspended = deleted = False
    try:
        login = browser.post(
            config.PLAMENU_URL + "/login",
            data={"email": plamenu_user.email, "password": plamenu_user.password},
            allow_redirects=False,
            timeout=30,
        )
        assert login.status_code == 303
        with tempfile.TemporaryDirectory() as temporary:
            movie = peertube.make_sample_mp4(
                Path(temporary) / "film.mp4", seconds=8, size="640x360"
            )
            draft = prepare(browser, channel, movie, f"Takedown {name}", "Steady tone")
        media_id = draft.rsplit("/", 1)[1]
        pt.post("/api/v1/users/me/subscriptions", uri=handle)
        subscribed = True
        film = api.post(
            "/api/v1/videos",
            channel_id=channel["id"],
            media_id=media_id,
            title=f"Takedown {name}",
            description="Original author survives ownership changes",
            visibility="public",
            language="en",
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
                    if v.get("name") == f"Takedown {name}"
                ),
                None,
            ),
            desc="PeerTube imports the moderation fixture",
        )
        attachment = film["media_attachments"][0]
        master_url = f"{config.PLAMENU_URL}/media/hls/{media_id}/master.m3u8"
        master = requests.get(master_url, verify=False, timeout=30)
        assert master.status_code == 200
        urls = [
            attachment["url"],
            attachment["preview_url"],
            attachment["caption_url"],
            attachment["storyboard"]["url"],
            master_url,
            f"{config.PLAMENU_URL}/media/hls/{media_id}/sha256.json",
            f"{config.PLAMENU_URL}/media/{media_id}/download",
            f"{config.PLAMENU_URL}/web/statuses/{film['id']}/embed?p2p=0",
        ]
        urls += [
            urljoin(config.PLAMENU_URL, line)
            for line in master.text.splitlines()
            if line.startswith("/media/hls/")
        ]
        body = requests.get(attachment["url"], verify=False, timeout=30)
        assert body.status_code == 200
        peer_file = pt.video(remote["uuid"])["files"][0]["fileUrl"]
        assert requests.get(peer_file, verify=False, timeout=30).content == body.content

        with step(
            "an accepted transfer preserves authorship; a video report reaches moderators"
        ):
            api.post(
                f"/api/v1/video_channels/{channel['id']}",
                operation="transfer",
                username=owner.username,
            )
            owner_api.post(
                f"/api/v1/video_channels/{channel['id']}",
                operation="accept_transfer",
                confirm=name,
            )
            transferred = True
            wait_for(
                lambda: (
                    pt.get(f"/api/v1/video-channels/{handle}")["ownerAccount"]["name"]
                    == owner.username
                ),
                desc="PeerTube acknowledges the new owner",
            )
            author_id = film["account"]["id"]
            report = owner_api.report(
                author_id,
                comment="Disposable native-video moderation check",
                category="other",
                status_ids=[film["id"]],
            )
            detail = admin.get(f"/api/v1/admin/reports/{report['id']}")
            assert any(s["id"] == film["id"] for s in detail["statuses"])
            admin.post(f"/api/v1/admin/reports/{report['id']}/resolve")
            assert (
                admin.get(f"/api/v1/admin/reports/{report['id']}")["action_taken"]
                is True
            )

        with step(
            "suspending the new owner immediately refuses all origin lanes, even with author cookies"
        ):
            admin.post(
                f"/api/v1/admin/accounts/{owner_id}/action",
                type="suspend",
                text="Disposable origin check",
            )
            suspended = True
            for client in (requests, browser):
                for url in urls:
                    response = client.get(url, verify=False, timeout=30)
                    assert response.status_code == 404, (url, response.status_code)
                    assert response.headers["Cache-Control"] == "private, no-store", url
            response = requests.get(
                peer_file, headers={"Range": "bytes=0-63"}, verify=False, timeout=30
            )
            assert response.status_code == 404, (
                "the actual PeerTube file offer must also fail at the origin"
            )

        with step(
            "reversible unsuspension restores exact bytes; owner deletion is acknowledged remotely"
        ):
            admin.post(f"/api/v1/admin/accounts/{owner_id}/unsuspend")
            suspended = False
            for url in urls:
                assert requests.get(url, verify=False, timeout=30).status_code == 200, (
                    url
                )
            assert (
                requests.get(attachment["url"], verify=False, timeout=30).content
                == body.content
            )
            owner_api.post(
                f"/api/v1/video_channels/{channel['id']}",
                operation="delete",
                confirm=name,
            )
            deleted = True
            wait_for(
                lambda: (
                    pt.http.get(
                        f"{pt.base_url}/api/v1/videos/{remote['uuid']}", timeout=30
                    ).status_code
                    == 404
                ),
                desc="PeerTube acknowledges original-author and current-owner retractions",
            )
            for url in urls:
                assert requests.get(url, verify=False, timeout=30).status_code == 404, (
                    url
                )
    finally:
        if suspended:
            admin.post(f"/api/v1/admin/accounts/{owner_id}/unsuspend")
        if not deleted:
            (owner_api if transferred else api).post(
                f"/api/v1/video_channels/{channel['id']}",
                operation="delete",
                confirm=name,
            )
        if draft:
            page = browser.get(draft, timeout=30)
            if page.status_code == 200:
                browser.post(
                    f"{config.PLAMENU_URL}/web/videos/drafts/{media_id}",
                    data={"csrf": csrf(page.text), "op": "cancel", "confirm": "yes"},
                    timeout=30,
                )
        if subscribed:
            pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30
            )
        browser.close()
