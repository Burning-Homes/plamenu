"""Funkwhale's canonical top-level Audio object ingests natively."""

import subprocess
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, funkwhale
from plamenu_e2e.api import Api
from plamenu_e2e.db import Db
from plamenu_e2e.funkwhale import FunkwhaleApi
from plamenu_e2e.steps import step, wait_for

pytestmark = pytest.mark.skipif(
    not funkwhale.reachable(), reason="funkwhale.local is not up"
)


def _make_audio(path: Path) -> None:
    subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-loglevel",
            "fatal",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=659:duration=2",
            "-q:a",
            "8",
            str(path),
        ],
        check=True,
    )


def _resolved_audio(api: Api, audio_id: str) -> dict | None:
    status = api.resolve_status(audio_id)
    if not status or not status["media_attachments"]:
        return None
    return status if status["media_attachments"][0].get("url") else None


@pytest.mark.federation(
    direction="inbound",
    peer="funkwhale",
    one_way_reason=(
        "Funkwhale is the native producer for top-level Audio; Plamenu does not "
        "yet offer a matching outbound post kind. Funkwhale 2.0.8 also leaves "
        "public delivery to non-Funkwhale follower inboxes disabled upstream, so "
        "the canonical AP object is exercised through explicit resolution."
    ),
)
def test_funkwhale_audio_resolves_with_title_duration_and_media(
    funkwhale_fiona: FunkwhaleApi,
    plamenu_api: Api,
    db: Db,
    marker: str,
    tmp_path: Path,
):
    """The producer's real Audio shape survives fetch, author and media ingest."""
    db.forget_host(config.FUNKWHALE_DOMAIN)
    source = tmp_path / f"{marker}.mp3"
    _make_audio(source)
    title = f"Funkwhale native Audio {marker}"

    with step("Fiona uploads a track to the standing Funkwhale channel"):
        upload = funkwhale_fiona.upload_and_wait(source, title)
        audio_id = upload["activitypub_id"]

    with step("the advertised audio is private to signed federation requests"):
        activitypub_audio = funkwhale_fiona.activitypub_audio(upload["uuid"])
        media_url = next(
            link["href"]
            for link in activitypub_audio["url"]
            if link.get("mediaType", "").startswith("audio/")
        )
        anonymous = requests.get(media_url, verify=False, timeout=30)
        assert anonymous.status_code == 401, anonymous.text[:300]

    with step("Plamenu explicitly resolves and ingests the canonical Audio object"):
        received = wait_for(
            lambda: _resolved_audio(plamenu_api, audio_id),
            desc="Plamenu to ingest Funkwhale's Audio",
        )
        assert received["uri"] == audio_id, received
        assert received["object_type"] == "Audio", received
        assert received["title"] == title, received
        assert received["account"]["acct"] == funkwhale.CHANNEL_HANDLE

    with step("the playable file and declared duration survive ingestion"):
        attachments = received["media_attachments"]
        assert len(attachments) == 1, attachments
        attachment = attachments[0]
        assert attachment["type"] == "audio", attachment
        # The background downloader may already have cached the signed media.
        assert attachment["url"].startswith(f"{config.PLAMENU_URL}/media/"), attachment
        assert attachment["meta"]["original"]["duration"] >= 1.5, attachment
        downloaded = requests.get(attachment["url"], verify=False, timeout=30)
        assert downloaded.status_code == 200, downloaded.text[:300]
        assert downloaded.headers["content-type"].startswith("audio/mpeg")
        assert len(downloaded.content) > 1_000
        assert db.remote_fetch_failures_for(config.FUNKWHALE_DOMAIN) == 0
