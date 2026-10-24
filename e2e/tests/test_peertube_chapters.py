"""Real PeerTube chapters through federated hasParts, API and accessible player."""

import os
import subprocess
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for


def _replace(pt, video_id, chapters):
    response = pt.http.put(
        pt.base_url + f"/api/v1/videos/{video_id}/chapters",
        json={
            "chapters": [
                {"timecode": start, "title": title} for start, title in chapters
            ]
        },
        timeout=60,
    )
    assert response.status_code == 204, response.text[:500]


@pytest.mark.requires_peer("peertube")
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    reverse_of="test_local_chapters_draft_publish_update_remove",
)
def test_peertube_chapters_import_seek_and_update(plamenu_api: Api):
    pt = peertube.root()
    created = None
    followed_root_id = None
    try:
        with step("the test viewer subscribes to PeerTube Video Updates"):
            root_actor = wait_for(
                lambda: plamenu_api.resolve_account("root@peertube.local"),
                desc="Plamenu resolves the PeerTube account",
            )
            # Resolution alone does not subscribe this instance to Updates.
            # plamenu_api belongs to this test's fresh disposable account.
            followed_root_id = root_actor["id"]
            plamenu_api.follow(followed_root_id)
            wait_for(
                lambda: plamenu_api.relationship(followed_root_id)["following"],
                desc="PeerTube accepts the test viewer's follow",
            )
        with step("PeerTube publishes a timed hasParts chapter collection"):
            with tempfile.TemporaryDirectory() as directory:
                sample = peertube.make_sample_mp4(
                    Path(directory) / "chapters.mp4", seconds=20
                )
                created = pt.upload_video(sample, unique("chaptered-peer"))
            pt.wait_for_hls(created["uuid"])
            _replace(
                pt,
                created["id"],
                [
                    (0, "Opening scene"),
                    (6, "The middle scene"),
                    (12, "The final scene"),
                ],
            )

            def ap_chapters():
                obj = pt.fetch_ap(created["uuid"])
                if not obj or not isinstance(obj.get("hasParts"), str):
                    return None
                response = pt.http.get(
                    obj["hasParts"],
                    headers={"Accept": "application/activity+json"},
                    timeout=30,
                )
                if response.status_code != 200:
                    return None
                parts = response.json().get("hasPart", [])
                return parts if len(parts) == 3 else None

            parts = wait_for(ap_chapters, desc="PeerTube AP chapters", timeout=90)
            assert [part["startOffset"] for part in parts] == [0, 6, 12]

        with step("Plamenu imports chapters and no-JS links retain media fragments"):

            def imported():
                status = plamenu_api.resolve_status(pt.ap_id(created["uuid"]))
                attachment = status["media_attachments"][0]
                chapters = attachment.get("chapters", [])
                return status if len(chapters) == 3 and attachment.get("url") else None

            status = wait_for(imported, desc="Plamenu chapters import", timeout=90)
            chapters = status["media_attachments"][0]["chapters"]
            assert chapters == [
                {"start_seconds": 0, "title": "Opening scene"},
                {"start_seconds": 6, "title": "The middle scene"},
                {"start_seconds": 12, "title": "The final scene"},
            ]
            watch_url = f"{config.PLAMENU_URL}/web/statuses/{status['id']}"
            wait_for(
                lambda: (
                    'data-chapter-seek="6"'
                    in requests.get(watch_url, verify=False, timeout=30).text
                ),
                desc="first-party chapter links rendered",
                timeout=60,
            )
            screenshot = config.PLAMENU_DIR / "target/e2e/peertube-chapters.png"
            screenshot.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                [
                    "node",
                    "verify-peertube-chapters.mjs",
                    watch_url,
                    "3",
                    "6",
                    str(screenshot),
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                check=True,
                timeout=120,
                env=os.environ.copy(),
            )

        with step("PeerTube chapter edit and removal clear stale Plamenu metadata"):
            _replace(pt, created["id"], [(0, "New opening"), (9, "New ending")])

            def updated():
                chapters = plamenu_api.get_status(status["id"])["media_attachments"][
                    0
                ].get("chapters", [])
                return (
                    chapters
                    if len(chapters) == 2 and chapters[1]["start_seconds"] == 9
                    else None
                )

            chapters = wait_for(updated, desc="chapter update federates", timeout=120)
            assert [part["title"] for part in chapters] == ["New opening", "New ending"]
            _replace(pt, created["id"], [])

            def cleared():
                chapters = plamenu_api.get_status(status["id"])["media_attachments"][
                    0
                ].get("chapters", [])
                return not chapters

            wait_for(cleared, desc="chapter removal federates", timeout=120)
    finally:
        try:
            if created is not None:
                pt.delete_video(created["uuid"])
        finally:
            if followed_root_id is not None:
                plamenu_api.unfollow(followed_root_id)
