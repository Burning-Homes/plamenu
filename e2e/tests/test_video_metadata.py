"""PeerTube metadata Create/Update interoperates without invented licence versions."""

import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import peertube, unique
from plamenu_e2e.steps import wait_for


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    one_way_reason="Inbound metadata create/update/clear refinement of the paired Video lifecycle; outbound publication/update/delete remains paired with the core inbound Video test.",
)
def test_peertube_video_metadata_create_update_and_clear(plamenu_api):
    pt = peertube.root()
    created = None
    account = None
    try:
        channel = pt.me()["videoChannels"][0]
        account = wait_for(
            lambda: plamenu_api.resolve_account(f"{channel['name']}@peertube.local"),
            desc="PeerTube metadata channel resolves",
        )
        plamenu_api.follow(account["id"])
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="PeerTube accepts metadata channel Follow",
        )
        with tempfile.TemporaryDirectory() as directory:
            sample = peertube.make_sample_mp4(
                Path(directory) / "metadata.mp4", seconds=5
            )
            created = pt.upload_video(sample, unique("metadata"), privacy=3)
        pt.put(
            f"/api/v1/videos/{created['uuid']}",
            category=13,
            licence=2,
            tags=["astronomy"],
            nsfw=True,
            nsfwSummary="Flashing imagery",
            description="A star lesson.",
            privacy=1,
        )
        pt.wait_for_hls(created["uuid"])
        status = wait_for(
            lambda: plamenu_api.resolve_status(pt.ap_id(created["uuid"])),
            desc="PeerTube metadata Video ingests",
        )
        media_id = status["media_attachments"][0]["id"]
        metadata = status["media_attachments"][0]["video_metadata"]
        assert metadata["category"] == {"id": 13, "name": "Education"}
        assert metadata["licence"] == {"id": 2, "name": "CC BY-SA", "url": None}
        assert status["sensitive"] is True
        assert status["spoiler_text"] == "Flashing imagery"
        assert any(tag["name"] == "astronomy" for tag in status["tags"])

        pt.put(f"/api/v1/videos/{created['uuid']}", category=15, licence=9)

        def updated():
            current = plamenu_api.get_status(status["id"])
            metadata = current["media_attachments"][0]["video_metadata"]
            return current if metadata["licence"]["id"] == 9 else None

        current = wait_for(updated, desc="PeerTube metadata-only signed Update ingests")
        assert current["media_attachments"][0]["id"] == media_id
        assert current["media_attachments"][0]["video_metadata"]["category"]["id"] == 15
        assert current["sensitive"] is True
        assert current["edited_at"] is not None
        # PeerTube's REST sanitizer explicitly accepts the string "null".
        pt.put(f"/api/v1/videos/{created['uuid']}", category="null", licence="null")
        detail = pt.video(created["uuid"])
        assert detail["category"]["id"] is None and detail["licence"]["id"] is None
        wait_for(
            lambda: (
                plamenu_api.get_status(status["id"])["media_attachments"][0][
                    "video_metadata"
                ]
                == {"category": None, "licence": None}
            ),
            desc="PeerTube clears metadata on Update",
        )
        pt.put(f"/api/v1/videos/{created['uuid']}", nsfw=False)
        wait_for(
            lambda: not plamenu_api.get_status(status["id"])["sensitive"],
            desc="PeerTube clears the independent content warning",
        )
        assert plamenu_api.get_status(status["id"])["spoiler_text"] == ""
    finally:
        if created is not None:
            pt.delete_video(created["uuid"])
        if account is not None:
            plamenu_api.unfollow(account["id"])
