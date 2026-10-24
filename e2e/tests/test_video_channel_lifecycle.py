"""Channel collaboration, handoff and deletion on real Plamenu / PeerTube."""

import subprocess
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, plamenu, unique
from plamenu_e2e.steps import wait_for


def change(api, channel, operation, **fields):
    return api.post(
        f"/api/v1/video_channels/{channel['id']}", operation=operation, **fields
    )


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="local channel roles and consent are not a remote authorization protocol",
)
def test_collaborative_channel_profile_transfer_and_delete(
    plamenu_api, plamenu_user, cli, db
):
    pt = peertube.root()
    bob = plamenu.User()
    cli.account_add(bob)
    bob_api = plamenu.login(bob)
    name = unique("handoff")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Team films"
    )
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    deleted = False
    subscribed = False
    try:
        pt.post("/api/v1/users/me/subscriptions", uri=handle)
        subscribed = True
        wait_for(
            lambda: db.follower_count(name) > 0,
            desc="PeerTube accepts team channel Follow",
        )
        change(
            plamenu_api,
            channel,
            "profile",
            display_name="Collaborative films",
            description="A native team channel.",
        )
        wait_for(
            lambda: (
                pt.get(f"/api/v1/video-channels/{handle}")["displayName"]
                == "Collaborative films"
            ),
            desc="PeerTube receives channel profile Update",
        )
        change(plamenu_api, channel, "invite", username=bob.username, role="publisher")
        invitations = bob_api.get("/api/v1/video_channel_invitations")
        assert any(
            i["channel_id"] == channel["id"]
            and i["role"] == "publisher"
            and not i["ownership_transfer"]
            for i in invitations
        )
        assert all(
            c["id"] != channel["id"] for c in bob_api.get("/api/v1/video_channels")
        )
        change(bob_api, channel, "accept")
        assert any(
            c["id"] == channel["id"] and c["role"] == "publisher"
            for c in bob_api.get("/api/v1/video_channels")
        )
        with tempfile.TemporaryDirectory() as directory:
            sample = peertube.make_sample_mp4(Path(directory) / "team.mp4", seconds=5)
            media = bob_api.upload_media(
                sample.read_bytes(),
                filename="team.mp4",
                mime="video/mp4",
                description="A moving test pattern with an audible tone",
            )
        title = f"Collaborator film {name}"
        video = bob_api.post(
            "/api/v1/videos",
            channel_id=channel["id"],
            media_id=media["id"],
            title=title,
            description="Published by a consenting collaborator.",
            language="en",
        )
        original = bob_api.ap_get(video["uri"].removeprefix(config.PLAMENU_URL))

        def imported():
            return next(
                (
                    v
                    for v in pt.get("/api/v1/users/me/subscriptions/videos", count=100)[
                        "data"
                    ]
                    if v["name"] == title
                ),
                None,
            )

        remote = wait_for(imported, desc="PeerTube imports collaborator-authored Video")
        detail = pt.video(remote["uuid"])
        assert detail["channel"]["name"] == name
        # PeerTube groups channel videos under the current channel owner, while
        # Plamenu retains the actual original uploader in its canonical object.
        assert original["attributedTo"][0]["id"].endswith(
            video["account"]["uri"].split(config.PLAMENU_URL)[-1]
        )
        file_url = detail["files"][0]["fileUrl"]
        response = requests.get(
            file_url, headers={"Range": "bytes=0-63"}, verify=False, timeout=30
        )
        assert response.status_code == 206 and len(response.content) == 64
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "received.mp4"
            response = requests.get(file_url, verify=False, timeout=30)
            response.raise_for_status()
            path.write_bytes(response.content)
            subprocess.run(
                [
                    "ffmpeg",
                    "-v",
                    "error",
                    "-i",
                    str(path),
                    "-t",
                    "1",
                    "-f",
                    "null",
                    "-",
                ],
                check=True,
            )
        # The owner can edit a collaborator's post without replacing its author.
        plamenu_api.put(f"/api/v1/statuses/{video['id']}", title="Owner correction")
        wait_for(
            lambda: pt.video(remote["uuid"])["name"] == "Owner correction",
            desc="PeerTube receives authorized owner edit signed by original Video author",
        )
        change(plamenu_api, channel, "transfer", username=bob.username)
        assert any(
            i["channel_id"] == channel["id"] and i["ownership_transfer"]
            for i in bob_api.get("/api/v1/video_channel_invitations")
        )
        assert (
            pt.get(f"/api/v1/video-channels/{handle}")["ownerAccount"]["name"]
            == plamenu_user.username
        )
        change(bob_api, channel, "accept_transfer", confirm=name)
        wait_for(
            lambda: (
                pt.get(f"/api/v1/video-channels/{handle}")["ownerAccount"]["name"]
                == bob.username
            ),
            desc="PeerTube receives accepted channel ownership Update",
        )
        assert all(
            c["id"] != channel["id"] for c in plamenu_api.get("/api/v1/video_channels")
        )
        denied = plamenu_api.http.put(
            f"{config.PLAMENU_URL}/api/v1/statuses/{video['id']}",
            data={"title": "Former owner edit"},
            timeout=30,
        )
        assert denied.status_code == 404
        bob_api.put(f"/api/v1/statuses/{video['id']}", title="After handoff")
        wait_for(
            lambda: pt.video(remote["uuid"])["name"] == "After handoff",
            desc="Video remains editable after channel handoff",
        )
        after = bob_api.ap_get(video["uri"].removeprefix(config.PLAMENU_URL))
        for key in ("id", "uuid", "attributedTo"):
            assert after[key] == original[key]
        change(bob_api, channel, "delete", confirm=name)
        deleted = True
        wait_for(
            lambda: (
                pt.http.get(
                    f"{pt.base_url}/api/v1/videos/{remote['uuid']}", timeout=30
                ).status_code
                == 404
            ),
            desc="PeerTube retracts deleted channel's Video",
        )
        assert (
            bob_api.http.get(
                f"{config.PLAMENU_URL}/api/v1/statuses/{video['id']}", timeout=30
            ).status_code
            == 404
        )
        assert (
            bob_api.get("/api/v1/accounts/verify_credentials")["username"]
            == bob.username
        )
    finally:
        if not deleted:
            owner_id = db._value(
                "SELECT owner_account_id FROM video_channels WHERE account_id=%s",
                channel["id"],
            )
            bob_id = bob_api.get("/api/v1/accounts/verify_credentials")["id"]
            change(
                bob_api if str(owner_id) == bob_id else plamenu_api,
                channel,
                "delete",
                confirm=name,
            )
        if subscribed:
            response = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30
            )
            assert response.status_code in (204, 404)


def test_channel_html_forms_require_csrf_and_private_cache(plamenu_api, plamenu_user):
    import re

    name = unique("channelweb")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Web channel"
    )
    session = requests.Session()
    session.verify = False
    session.post(
        f"{config.PLAMENU_URL}/login",
        data={"email": plamenu_user.email, "password": plamenu_user.password},
        timeout=30,
    )
    path = f"{config.PLAMENU_URL}/videos/channels/{channel['id']}"
    page = session.get(path, timeout=30)
    page.raise_for_status()
    assert page.headers["Cache-Control"] == "private, no-store"
    assert page.headers["Vary"] == "Cookie"
    csrf = re.search(r'name="csrf" value="([^"]+)"', page.text)[1]
    endpoint = f"{config.PLAMENU_URL}/web/video-channels/{channel['id']}"
    assert (
        session.post(
            endpoint,
            data={"operation": "profile", "display_name": "Forgery"},
            timeout=30,
        ).status_code
        == 403
    )
    response = session.post(
        endpoint,
        data={
            "csrf": csrf,
            "operation": "profile",
            "display_name": "No-script edit",
            "description": "<script>not markup</script>",
        },
        allow_redirects=False,
        timeout=30,
    )
    assert response.status_code == 303
    page = session.get(path, timeout=30)
    assert "&lt;script&gt;not markup&lt;/script&gt;" in page.text
    assert requests.get(
        path, verify=False, allow_redirects=False, timeout=30
    ).status_code in (303, 401)
    response = session.post(
        endpoint,
        data={"csrf": csrf, "operation": "delete", "confirm": name},
        allow_redirects=False,
        timeout=30,
    )
    assert response.status_code == 303


def test_published_video_delete_requires_no_js_confirmation(plamenu_api, plamenu_user):
    import re

    name = unique("confirmedvideo")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Confirmed videos"
    )
    with tempfile.TemporaryDirectory() as directory:
        sample = peertube.make_sample_mp4(Path(directory) / "confirm.mp4", seconds=3)
        media = plamenu_api.upload_media(
            sample.read_bytes(), filename="confirm.mp4", mime="video/mp4"
        )
    video = plamenu_api.post(
        "/api/v1/videos",
        channel_id=channel["id"],
        media_id=media["id"],
        title="Delete with confirmation",
    )
    session = requests.Session()
    session.verify = False
    session.post(
        f"{config.PLAMENU_URL}/login",
        data={"email": plamenu_user.email, "password": plamenu_user.password},
        timeout=30,
    )
    page = session.get(
        f"{config.PLAMENU_URL}/videos/{media['id']}/metadata", timeout=30
    )
    page.raise_for_status()
    assert 'action="/web/confirm"' in page.text
    csrf = re.search(r'name="csrf" value="([^"]+)"', page.text)[1]
    action = f"/web/statuses/{video['id']}/delete"
    confirmation = session.post(
        f"{config.PLAMENU_URL}/web/confirm",
        data={
            "csrf": csrf,
            "confirm_action": action,
            "confirm_message": "Delete this post?",
            "return_to": "/videos",
        },
        timeout=30,
    )
    confirmation.raise_for_status()
    assert f'action="{action}"' in confirmation.text
    assert plamenu_api.get_status(video["id"])["id"] == video["id"]
    response = session.post(
        f"{config.PLAMENU_URL}{action}",
        data={"csrf": csrf, "return_to": "/videos"},
        allow_redirects=False,
        timeout=30,
    )
    assert response.status_code == 303
    assert (
        plamenu_api.http.get(
            f"{config.PLAMENU_URL}/api/v1/statuses/{video['id']}", timeout=30
        ).status_code
        == 404
    )
    change(plamenu_api, channel, "delete", confirm=name)
