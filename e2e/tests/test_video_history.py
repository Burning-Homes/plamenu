"""Private watch positions on real native and PeerTube Video identities."""

import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import peertube, unique
from plamenu_e2e.steps import wait_for


def _enable(api, enabled):
    return api._request(
        "PUT", "/api/v1/video_viewer_preferences", json={"history_enabled": enabled}
    )


def _save(api, media, seconds):
    return api._request(
        "PUT", f"/api/v1/video_history/{media}", json={"position_seconds": seconds}
    )


def test_native_video_private_history_opt_in_forget_disable_and_deletion(plamenu_api):
    api = plamenu_api
    channel = api.post(
        "/api/v1/video_channels",
        username=unique("history")[:30],
        display_name="History test films",
    )
    posted = None
    try:
        with tempfile.TemporaryDirectory() as temp:
            sample = peertube.make_sample_mp4(Path(temp) / "history.mp4", seconds=12)
            media = api.upload_media(
                sample.read_bytes(),
                filename="history.mp4",
                mime="video/mp4",
                description="A test pattern.",
            )
        posted = api.post(
            "/api/v1/videos",
            channel_id=channel["id"],
            media_id=media["id"],
            title="Private history native Video",
            visibility="unlisted",
        )
        assert api.get("/api/v1/video_viewer_preferences")["history_enabled"] is False
        assert _save(api, media["id"], 5) == {"saved": False}
        assert api.get("/api/v1/video_history")["data"] == []
        _enable(api, True)
        assert _save(api, media["id"], 5) == {"saved": True}
        history = api.get("/api/v1/video_history")["data"]
        assert len(history) == 1 and history[0]["media_id"] == media["id"]
        assert history[0]["resume_url"].endswith("#t=5")
        assert (
            "viewer_history" not in api.get_status(posted["id"])["media_attachments"][0]
        )
        api.delete(f"/api/v1/video_history/{media['id']}")
        assert api.get("/api/v1/video_history")["data"] == []
        _save(api, media["id"], 6)
        _enable(api, False)
        assert api.get("/api/v1/video_history")["data"] == []
        _enable(api, True)
        assert api.get("/api/v1/video_history")["data"] == []
        _save(api, media["id"], 6)
        api.delete_status(posted["id"])
        posted = None
        wait_for(
            lambda: api.get("/api/v1/video_history")["data"] == [],
            desc="deleted native Video is absent from private history",
        )
    finally:
        _enable(api, False)
        if posted:
            api.delete_status(posted["id"])
        api.post(
            f"/api/v1/video_channels/{channel['id']}",
            operation="delete",
            confirm=channel["username"],
        )


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="private viewer history is deliberately never federated",
)
def test_peertube_video_history_survives_update_but_not_delete(plamenu_api):
    api = plamenu_api
    pt = peertube.root()
    created = None
    followed = None
    try:
        channel = pt.me()["videoChannels"][0]
        followed = wait_for(
            lambda: api.resolve_account(f"{channel['name']}@peertube.local"),
            desc="resolve PeerTube history channel",
        )
        api.follow(followed["id"])
        wait_for(
            lambda: api.relationship(followed["id"])["following"],
            desc="PeerTube accepts history Follow",
        )
        with tempfile.TemporaryDirectory() as temp:
            sample = peertube.make_sample_mp4(
                Path(temp) / "history-remote.mp4", seconds=12
            )
            created = pt.upload_video(sample, unique("history"), privacy=1)
        pt.wait_for_hls(created["uuid"])
        post = wait_for(
            lambda: api.resolve_status(pt.ap_id(created["uuid"])),
            desc="ingest history PeerTube Video",
        )
        media = post["media_attachments"][0]
        _enable(api, True)
        assert _save(api, media["id"], 5)["saved"]
        pt.put(f"/api/v1/videos/{created['uuid']}", name="Edited PeerTube history film")
        current = wait_for(
            lambda: (
                api.get_status(post["id"])
                if api.get_status(post["id"])["title"] == "Edited PeerTube history film"
                else None
            ),
            desc="signed PeerTube title Update",
        )
        assert current["media_attachments"][0]["id"] == media["id"]
        history = api.get("/api/v1/video_history")["data"]
        assert history[0]["position_seconds"] == 5
        assert history[0]["title"] == "Edited PeerTube history film"
        # Fetch only the actual resumed range, not the whole origin movie.
        response = api.http.get(
            history[0]["resume_url"].split("#")[0],
            headers={"Range": "bytes=0-4095"},
            timeout=30,
        )
        assert response.status_code == 206 and len(response.content) == 4096
        pt.delete_video(created["uuid"])
        created = None
        wait_for(
            lambda: api.get("/api/v1/video_history")["data"] == [],
            desc="PeerTube Delete revokes private history entry",
        )
    finally:
        _enable(api, False)
        if created:
            pt.delete_video(created["uuid"])
        if followed:
            api.unfollow(followed["id"])
