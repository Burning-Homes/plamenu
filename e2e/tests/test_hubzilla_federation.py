"""Hubzilla's native top-level file publications ingest without conversion."""

import re
import subprocess
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, hubzilla
from plamenu_e2e.api import Api
from plamenu_e2e.db import Db
from plamenu_e2e.steps import step, wait_for

pytestmark = pytest.mark.skipif(
    not hubzilla.reachable(), reason="hubzilla.local is not up"
)


def _make_native_files(
    root: Path, marker: str
) -> list[tuple[str, Path, str, str, str]]:
    """Create one valid file for every object type Hubzilla publishes."""
    image = root / f"{marker}.jpg"
    audio = root / f"{marker}.mp3"
    video = root / f"{marker}.mp4"
    document = root / f"{marker}.md"
    commands = [
        [
            "ffmpeg",
            "-nostdin",
            "-loglevel",
            "fatal",
            "-f",
            "lavfi",
            "-i",
            "color=c=0x4169e1:size=64x48",
            "-frames:v",
            "1",
            str(image),
        ],
        [
            "ffmpeg",
            "-nostdin",
            "-loglevel",
            "fatal",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=523:duration=1",
            "-q:a",
            "8",
            str(audio),
        ],
        [
            "ffmpeg",
            "-nostdin",
            "-loglevel",
            "fatal",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x48:rate=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            str(video),
        ],
    ]
    for command in commands:
        subprocess.run(command, check=True)
    document.write_text(f"# Native Hubzilla document\n\n{marker}\n", encoding="utf-8")
    return [
        ("Image", image, "image", "image/", "shared an image"),
        ("Audio", audio, "audio", "audio/", "shared a file"),
        ("Video", video, "video", "video/", "shared a file"),
        ("Document", document, "unknown", "text/markdown", "shared a file"),
    ]


def _home_status_named(api: Api, title: str) -> dict | None:
    status = next(
        (status for status in api.home_timeline(limit=40) if status["title"] == title),
        None,
    )
    if not status or not status["media_attachments"]:
        return None
    return status if status["media_attachments"][0].get("url") else None


@pytest.mark.federation(
    direction="inbound",
    peer="hubzilla",
    one_way_reason=(
        "Hubzilla is the native producer for top-level Image/Audio/Video/Document; "
        "Plamenu does not yet offer matching outbound post kinds, which is the next "
        "phase this ingestion coverage enables."
    ),
)
def test_hubzilla_native_file_objects_arrive_without_loss(
    hubzilla_hazel: dict,
    plamenu_api: Api,
    db: Db,
    marker: str,
    tmp_path: Path,
):
    """Signed Create delivery preserves type, native body and primary file."""
    db.forget_host(config.HUBZILLA_DOMAIN)

    with step("Plamenu resolves and follows Hazel's public Hubzilla channel"):
        remote = plamenu_api.resolve_account(hubzilla.HAZEL_HANDLE)
        assert remote and remote["uri"] == hubzilla_hazel["id"], remote
        plamenu_api.follow(remote["id"])
        wait_for(
            lambda: plamenu_api.relationship(remote["id"])["following"],
            desc="Hubzilla Accept(Follow) to reach Plamenu",
        )

    for (
        object_type,
        path,
        attachment_type,
        media_type,
        body_phrase,
    ) in _make_native_files(tmp_path, marker):
        remote_name = f"{object_type.lower()}-{marker}{path.suffix}"
        with step(f"Hubzilla publishes a native {object_type}"):
            published = hubzilla.upload_and_publish(path, remote_name=remote_name)
            assert published["type"] == object_type, published

        with step(f"the {object_type} arrives as itself with its native body and file"):
            received = wait_for(
                lambda remote_name=remote_name: _home_status_named(
                    plamenu_api, remote_name
                ),
                desc=f"Hubzilla {object_type} delivery to reach the home timeline",
            )
            assert received["uri"] == published["id"], received
            assert received["object_type"] == object_type, received
            assert received["account"]["acct"] == hubzilla.HAZEL_HANDLE

            # The entity renderer hoists `name` into one visible title. The
            # native share sentence must remain underneath it; the old compact
            # Document conversion rendered the title twice and lost this body.
            visible = re.sub(r"<[^>]+>", "", received["content"])
            assert visible.count(remote_name) == 1, visible
            assert body_phrase in visible, visible

            attachments = received["media_attachments"]
            assert len(attachments) == 1, attachments
            assert attachments[0]["type"] == attachment_type, attachments[0]
            # Background processing may cache the file before the timeline
            # poll returns; both cached files and proxy URLs are served locally.
            assert attachments[0]["url"].startswith(f"{config.PLAMENU_URL}/media/"), (
                attachments[0]
            )
            downloaded = requests.get(attachments[0]["url"], verify=False, timeout=30)
            assert downloaded.status_code == 200, downloaded.text[:300]
            assert downloaded.headers["content-type"].startswith(media_type)
            assert len(downloaded.content) > 10
            if object_type == "Document":
                assert marker in downloaded.text
