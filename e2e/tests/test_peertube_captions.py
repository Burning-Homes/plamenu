"""PeerTube caption languages through AP, same-origin proxy and native tracks."""

import os
import subprocess
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for


def _caption(pt, video_id, language, text):
    body = (
        f"WEBVTT\n\n00:00:00.000 --> 00:00:15.000\n{text}\n\n"
        f"00:00:16.000 --> 00:00:19.000\nClosing {text}\n"
    ).encode()
    response = pt.http.put(
        pt.base_url + f"/api/v1/videos/{video_id}/captions/{language}",
        files={"captionfile": (f"{language}.vtt", body, "text/vtt")},
        timeout=60,
    )
    assert response.status_code == 204, response.text[:500]


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="inbound",
    peer="peertube",
    reverse_of="test_local_caption_languages_review_publish_and_play",
)
def test_peertube_caption_languages_play_without_js_and_update(plamenu_api: Api):
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
        with step("PeerTube publishes two language-specific WebVTT tracks"):
            with tempfile.TemporaryDirectory() as directory:
                sample = peertube.make_sample_mp4(
                    Path(directory) / "captioned.mp4", seconds=20
                )
                created = pt.upload_video(sample, unique("captioned-peer"))
            pt.wait_for_hls(created["uuid"])
            _caption(pt, created["id"], "en", "Hello from PeerTube")
            _caption(pt, created["id"], "fr", "Bonjour de PeerTube")
            wait_for(
                lambda: (
                    (ap if len(ap.get("subtitleLanguage", [])) == 2 else None)
                    if (ap := pt.fetch_ap(created["uuid"]))
                    else None
                ),
                desc="PeerTube ActivityPub object carries two captions",
                timeout=90,
            )

        with step("Plamenu imports both tracks without exposing the origin URL"):

            def imported():
                status = plamenu_api.resolve_status(pt.ap_id(created["uuid"]))
                tracks = status["media_attachments"][0].get("captions", [])
                return status if len(tracks) == 2 else None

            status = wait_for(
                imported, desc="both caption languages import", timeout=90
            )
            attachment = status["media_attachments"][0]
            tracks = attachment["captions"]
            assert [track["language"] for track in tracks] == ["en", "fr"]
            assert all(
                track["url"].startswith(config.PLAMENU_URL + "/media/")
                for track in tracks
            )
            assert all(
                track["transcript_url"].startswith(config.PLAMENU_URL + "/videos/")
                for track in tracks
            )
            assert attachment["caption_url"] == tracks[0]["url"]
            for language, text in (
                ("en", "Hello from PeerTube"),
                ("fr", "Bonjour de PeerTube"),
            ):

                def ready_track(language=language):
                    # PeerTube can publish another Video Update while its HLS
                    # ladder settles. Plamenu may replace the media row, so
                    # resolve the current attachment before testing its URL.
                    current = plamenu_api.get_status(status["id"])
                    current_tracks = current["media_attachments"][0].get("captions", [])
                    track = next(
                        (
                            item
                            for item in current_tracks
                            if item["language"] == language
                        ),
                        None,
                    )
                    if track is None:
                        return None
                    response = requests.get(track["url"], verify=False, timeout=30)
                    return (track, response) if response.status_code == 200 else None

                track, response = wait_for(
                    ready_track,
                    desc=f"current PeerTube {language} caption becomes available",
                    timeout=90,
                )
                assert response.status_code == 200 and text in response.text
                assert response.headers["Content-Type"].startswith("text/vtt")
                assert response.headers["Cache-Control"] == "no-store"
                transcript = requests.get(
                    track["transcript_url"], verify=False, timeout=30
                )
                assert transcript.status_code == 200, transcript.text[:500]
                assert text in transcript.text
                assert "#t=16.000" in transcript.text
                assert "This text is derived from timed captions" in transcript.text
                assert "peertube.local" not in transcript.text
                assert transcript.headers["Cache-Control"] == "private, no-store"

        with step(
            "no-JS and JS first-party players expose and play the language tracks"
        ):
            subprocess.run(
                [
                    "node",
                    "verify-peertube-captions.mjs",
                    f"{config.PLAMENU_URL}/web/statuses/{status['id']}",
                    "Bonjour de PeerTube",
                    '["en","fr"]',
                    "16000",
                    "Closing Bonjour de PeerTube",
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                check=True,
                timeout=120,
                env=os.environ.copy(),
            )

        with step(
            "a PeerTube Video edit keeps existing media and transcript URLs stable"
        ):
            current = plamenu_api.get_status(status["id"])
            original_media = current["media_attachments"][0]
            original_id = original_media["id"]
            old_track = next(
                track
                for track in original_media["captions"]
                if track["language"] == "en"
            )
            edited_title = unique("captioned-peer-edited")
            pt.put(f"/api/v1/videos/{created['id']}", name=edited_title)
            edited = wait_for(
                lambda: (
                    current
                    if (current := plamenu_api.get_status(status["id"]))["title"]
                    == edited_title
                    else None
                ),
                desc="PeerTube Video title Update reaches Plamenu",
                timeout=120,
            )
            assert edited["media_attachments"][0]["id"] == original_id
            assert (
                requests.get(old_track["url"], verify=False, timeout=30).status_code
                == 200
            )
            assert (
                requests.get(
                    old_track["transcript_url"], verify=False, timeout=30
                ).status_code
                == 200
            )

        with step("a PeerTube caption deletion removes the stale Plamenu track"):
            pt.delete(f"/api/v1/videos/{created['id']}/captions/fr")

            def one_track():
                current = plamenu_api.get_status(status["id"])
                tracks = current["media_attachments"][0].get("captions", [])
                return tracks if len(tracks) == 1 else None

            remaining = wait_for(
                one_track, desc="caption update federates", timeout=120
            )
            assert remaining[0]["language"] == "en"
            assert (
                plamenu_api.get_status(status["id"])["media_attachments"][0]["id"]
                == original_id
            )
            assert (
                requests.get(tracks[1]["url"], verify=False, timeout=30).status_code
                == 404
            )
            assert (
                requests.get(
                    tracks[1]["transcript_url"], verify=False, timeout=30
                ).status_code
                == 404
            )
    finally:
        try:
            if created is not None:
                pt.delete_video(created["uuid"])
        finally:
            if followed_root_id is not None:
                plamenu_api.unfollow(followed_root_id)
