"""Real Plamenu publication accepted by the pinned PeerTube peer.

This is the native publication slice, not a claim of long-form/live parity.
"""

import hashlib
import os
import re
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import urljoin

import pytest
import requests
from plamenu_e2e import config, peertube, unique
from plamenu_e2e.api import Api
from plamenu_e2e.db import Db
from plamenu_e2e.steps import step, wait_for

CSRF_RE = re.compile(r'name="csrf" value="([^"]+)"')


def _csrf(html: str) -> str:
    match = CSRF_RE.search(html)
    assert match, "first-party form has no CSRF token"
    return match.group(1)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    reverse_of="test_peertube_video_federates_and_plays_through_the_hls_proxy",
)
@pytest.mark.parametrize(
    "subscribe_before", [True, False], ids=["new-publication", "channel-history"]
)
def test_native_video_channel_publication_update_and_delete(
    plamenu_api: Api, db: Db, subscribe_before
):
    pt = peertube.root()
    name = unique("film")[:30]
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Native films"
    )
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    published = None
    try:
        if subscribe_before:
            with step("PeerTube subscribes to a Plamenu-owned video channel"):
                pt.post("/api/v1/users/me/subscriptions", uri=handle)
                wait_for(
                    lambda: db.follower_count(name) > 0,
                    desc="channel Follow reaches Plamenu",
                )

        with step("Plamenu publishes a native Video, not an attachment Note"):
            with tempfile.TemporaryDirectory() as temp:
                sample = peertube.make_sample_mp4(Path(temp) / "native.mp4", seconds=5)
                media = plamenu_api.upload_media(
                    sample.read_bytes(),
                    filename="native.mp4",
                    mime="video/mp4",
                    description="A test pattern with an audible tone",
                )
            published = plamenu_api.post(
                "/api/v1/videos",
                channel_id=channel["id"],
                media_id=media["id"],
                title=f"Native video {name}",
                description="Published from Plamenu. " * 40 + "*literal asterisks*",
                language="en",
            )
            assert published["object_type"] == "Video", published
            assert published["media_attachments"][0]["type"] == "video"

        if not subscribe_before:
            with step(
                "PeerTube subscribes after publication and imports signed channel history"
            ):
                pt.post("/api/v1/users/me/subscriptions", uri=handle)
                wait_for(
                    lambda: db.follower_count(name) > 0,
                    desc="late channel Follow reaches Plamenu",
                )

        with step("channel delivery appears in PeerTube without URL-search importing"):

            def delivered():
                videos = pt.get(
                    "/api/v1/users/me/subscriptions/videos",
                    count=100,
                    sort="-publishedAt",
                )["data"]
                return next(
                    (v for v in videos if v["name"] == f"Native video {name}"), None
                )

            remote = wait_for(delivered, desc="PeerTube receives channel Announce")
            detail = pt.video(remote["uuid"])
            assert detail["channel"]["name"] == name, detail
            assert detail["channel"]["host"] == config.PLAMENU_DOMAIN, detail
            assert detail["isLocal"] is False
            # PeerTube 8.3 includes the full description in video details;
            # the former /description subresource no longer exists.
            description = detail["description"]
            assert "Published from Plamenu. " * 39 in description
            assert "*literal asterisks*" in description
            files = detail["files"]
            assert files, detail
            # PeerTube 8.3's legacy thumbnailPath selects only 16:9; a
            # correctly preserved 4:3/portrait poster lives in thumbnails.
            thumbnails = detail["thumbnails"]
            assert thumbnails, detail
            assert any(t["aspectRatio"] == "16:9" for t in thumbnails), thumbnails
            assert thumbnails[0]["width"] > 0 and thumbnails[0]["height"] > 0
            poster = requests.get(thumbnails[0]["fileUrl"], verify=False, timeout=20)
            poster.raise_for_status()
            assert poster.headers["Content-Type"].startswith("image/"), poster.headers
            assert len(poster.content) > 100

        with step(
            "the actual video URL offered by PeerTube supports ranges and decoding"
        ):
            file_url = files[0]["fileUrl"]
            ranged = requests.get(
                file_url, headers={"Range": "bytes=0-63"}, verify=False, timeout=20
            )
            assert ranged.status_code == 206, ranged.status_code
            assert len(ranged.content) == 64
            with tempfile.TemporaryDirectory() as temp:
                media_path = Path(temp) / "received.mp4"
                response = requests.get(file_url, verify=False, timeout=30)
                response.raise_for_status()
                media_path.write_bytes(response.content)
                subprocess.run(
                    [
                        "ffmpeg",
                        "-v",
                        "error",
                        "-i",
                        str(media_path),
                        "-t",
                        "1",
                        "-f",
                        "null",
                        "-",
                    ],
                    check=True,
                    timeout=30,
                )

        with step("an owner edit reaches a channel-only subscriber"):
            plamenu_api._request(
                "PUT",
                f"/api/v1/statuses/{published['id']}",
                json={"title": f"Corrected {name}", "status": "x"},
            )
            wait_for(
                lambda: pt.video(remote["uuid"])["name"] == f"Corrected {name}",
                desc="PeerTube receives Video Update",
            )
            assert pt.video(remote["uuid"])["description"] == "<p>x</p>"

        with step("deletion removes the remote video"):
            plamenu_api.delete_status(published["id"])
            published = None
            wait_for(
                lambda: (
                    pt.http.get(
                        f"{pt.base_url}/api/v1/videos/{remote['uuid']}", timeout=15
                    ).status_code
                    == 404
                ),
                desc="PeerTube receives Video Delete",
            )
    finally:
        if published is not None:
            plamenu_api.delete_status(published["id"])
        response = pt.http.delete(
            f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30
        )
        assert response.status_code in (204, 404), response.text


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="same-direction refinement of the native-video publication test: this case uses the first-party durable draft uploader",
)
@pytest.mark.parametrize(
    "upload_mode", ["multipart", "resumable", "audio-only", "audio-resumable", "poster-frame", "custom-poster"]
)
def test_first_party_draft_upload_reaches_peertube(
    plamenu_api: Api, plamenu_user, upload_mode
):
    """Real HTML upload -> durable worker -> review -> PeerTube import/playback."""
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("draftfilm")[:30]
    title = f"Draft film {name}"
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Draft films"
    )
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    draft_url = None
    published_id = None
    subscribed = False
    try:
        with step("PeerTube follows the browser author's video channel"):
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True

        with step(f"the first-party {upload_mode} uploader stores a private original"):
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303, login.status_code
            compose = browser.get(f"{config.PLAMENU_URL}/videos", timeout=30)
            compose.raise_for_status()
            csrf = _csrf(compose.text)
            with tempfile.TemporaryDirectory() as temp:
                audio_only = upload_mode.startswith("audio-")
                if audio_only:
                    visual_description = "A blue waveform on a dark background"
                    sample = peertube.make_sample_audio(Path(temp) / "draft.m4a")
                elif upload_mode in ("poster-frame", "custom-poster"):
                    visual_description = "A red opening followed by blue frames"
                    sample = peertube.make_two_colour_video(Path(temp) / "draft.mp4")
                else:
                    visual_description = "Moving colour test pattern"
                    sample = peertube.make_sample_mp4(
                        Path(temp) / "draft.mp4", seconds=5, size="640x480"
                    )
                contents = sample.read_bytes()
                fields = [
                    ("csrf", (None, csrf)),
                    ("channel", (None, str(channel["id"]))),
                    ("title", (None, title)),
                    ("description", (None, "A film created with the draft uploader.")),
                    ("language", (None, "en")),
                    ("visibility", (None, "public")),
                    ("poster_time", (None, "3" if upload_mode == "poster-frame" else "0")),
                ]
                if upload_mode not in ("resumable", "audio-resumable"):
                    uploaded = browser.post(
                        f"{config.PLAMENU_URL}/web/videos",
                        files=fields
                        + [
                            (
                                "file",
                                (
                                    sample.name,
                                    contents,
                                    "audio/mp4" if audio_only else "video/mp4",
                                ),
                            ),
                            ("alt", (None, visual_description)),
                            (
                                "captions",
                                (
                                    "captions.vtt",
                                    b"WEBVTT\n\n00:00:00.000 --> 00:00:05.000\n[Test tone]\n",
                                    "text/vtt",
                                ),
                            ),
                            ("transcript", (None, "A steady test tone plays.")),
                            ("op", (None, "review")),
                        ],
                        allow_redirects=False,
                        timeout=60,
                    )
                    assert uploaded.status_code == 303, uploaded.text[:500]
                    draft_url = uploaded.headers["Location"]
                else:
                    edge = min(64 * 1024, len(contents))
                    fingerprint = hashlib.sha256(
                        contents[:edge] + contents[-edge:]
                    ).hexdigest()
                    initiated = browser.post(
                        f"{config.PLAMENU_URL}/web/videos/resumable",
                        files=fields
                        + [
                            ("alt", (None, visual_description)),
                            (
                                "captions",
                                (
                                    "captions.vtt",
                                    b"WEBVTT\n\n00:00:00.000 --> 00:00:05.000\n[Test tone]\n",
                                    "text/vtt",
                                ),
                            ),
                            ("transcript", (None, "A steady test tone plays.")),
                            ("source_size", (None, str(len(contents)))),
                            ("source_fingerprint", (None, fingerprint)),
                        ],
                        timeout=30,
                    )
                    assert initiated.status_code == 200, initiated.text[:500]
                    media_id = initiated.json()["id"]
                    assert isinstance(media_id, str) and media_id.isdecimal()
                    draft_url = f"/videos/drafts/{media_id}"
                    source_url = (
                        f"{config.PLAMENU_URL}/web/videos/drafts/{media_id}/source"
                    )
                    halfway = len(contents) // 2
                    for offset, chunk in [
                        (0, contents[:halfway]),
                        (halfway, contents[halfway:]),
                    ]:
                        part = browser.patch(
                            source_url,
                            headers={
                                "x-csrf-token": csrf,
                                "x-upload-offset": str(offset),
                            },
                            data=chunk,
                            timeout=30,
                        )
                        assert part.status_code == 200, part.text[:500]
                        progress = browser.get(source_url, timeout=30)
                        progress.raise_for_status()
                        assert progress.json()["offset"] == offset + len(chunk)
                    finished = browser.post(
                        f"{source_url}/finish",
                        headers={"x-csrf-token": csrf},
                        timeout=30,
                    )
                    assert finished.status_code == 200, finished.text[:500]
            assert re.fullmatch(r"/videos/drafts/\d+", draft_url), draft_url
            media_id = draft_url.rsplit("/", 1)[1]
            original = browser.get(
                f"{config.PLAMENU_URL}/media/{media_id}.orig", timeout=30
            )
            assert original.status_code == 404

        with step("the built-in worker finishes before explicit review"):

            def ready():
                response = browser.get(
                    f"{config.PLAMENU_URL}{draft_url}/status", timeout=30
                )
                response.raise_for_status()
                phase = response.json()["phase"]
                assert phase != "failed", "background video processing failed"
                return phase == "ready"

            wait_for(ready, desc="private draft is ready to review")
            review = browser.get(f"{config.PLAMENU_URL}{draft_url}/review", timeout=30)
            review.raise_for_status()
            assert visual_description in review.text
            if upload_mode == "poster-frame":
                assert "Poster frame at" in review.text
                assert "3 s" in review.text
            if upload_mode == "custom-poster":
                with tempfile.TemporaryDirectory() as temp:
                    poster = peertube.make_solid_poster(Path(temp) / "custom.png")
                    replaced = browser.post(
                        f"{config.PLAMENU_URL}/web/videos/drafts/{media_id}/poster",
                        files=[
                            ("csrf", (None, _csrf(review.text))),
                            ("poster_alt", (None, "A solid blue square.")),
                            ("poster_file", ("custom.png", poster.read_bytes(), "image/png")),
                        ],
                        allow_redirects=False,
                        timeout=60,
                    )
                assert replaced.status_code == 303, replaced.text[:500]
                review = browser.get(f"{config.PLAMENU_URL}{draft_url}/review", timeout=30)
                review.raise_for_status()
                assert "Custom poster" in review.text
                assert "A solid blue square." in review.text
            assert "A steady test tone plays." in review.text
            preview = browser.get(
                f"{config.PLAMENU_URL}{draft_url}/preview/video",
                headers={"Range": "bytes=0-63"},
                timeout=30,
            )
            assert preview.status_code == 206
            assert len(preview.content) == 64

        with step("review publishes one interoperable Video"):
            published = browser.post(
                f"{config.PLAMENU_URL}/web/videos",
                files=[
                    ("csrf", (None, _csrf(review.text))),
                    ("channel", (None, str(channel["id"]))),
                    ("media_id", (None, media_id)),
                    ("title", (None, title)),
                    ("description", (None, "A film created with the draft uploader.")),
                    ("language", (None, "en")),
                    ("visibility", (None, "public")),
                    ("op", (None, "publish")),
                ],
                allow_redirects=False,
                timeout=60,
            )
            assert published.status_code == 303, published.text[:500]
            published_id = published.headers["Location"].rsplit("/", 1)[1]
            status = plamenu_api.get(f"/api/v1/statuses/{published_id}")
            assert status["object_type"] == "Video", status
            assert status["media_attachments"][0]["type"] == "video", status
            if upload_mode == "custom-poster":
                assert status["media_attachments"][0]["poster_description"] == "A solid blue square."
            assert (
                status["media_attachments"][0]["description"]
                == visual_description
            )
            assert (
                browser.get(
                    f"{config.PLAMENU_URL}/media/{media_id}.orig", timeout=30
                ).status_code
                == 404
            )

        with step("PeerTube receives and decodes the draft-originated film"):

            def delivered():
                videos = pt.get(
                    "/api/v1/users/me/subscriptions/videos",
                    count=100,
                    sort="-publishedAt",
                )["data"]
                return next((video for video in videos if video["name"] == title), None)

            remote = wait_for(delivered, desc="PeerTube imports draft-originated Video")
            detail = pt.video(remote["uuid"])
            if upload_mode in ("poster-frame", "custom-poster"):
                thumbnail_url = urljoin(pt.base_url, detail["thumbnailPath"])
                thumbnail = pt.http.get(thumbnail_url, timeout=30)
                thumbnail.raise_for_status()
                with tempfile.TemporaryDirectory() as temp:
                    thumbnail_file = Path(temp) / "peer-poster.jpg"
                    thumbnail_file.write_bytes(thumbnail.content)
                    pixel = subprocess.run(
                        [
                            "ffmpeg",
                            "-v",
                            "error",
                            "-i",
                            str(thumbnail_file),
                            "-vf",
                            "scale=1:1",
                            "-pix_fmt",
                            "rgb24",
                            "-frames:v",
                            "1",
                            "-f",
                            "rawvideo",
                            "-",
                        ],
                        capture_output=True,
                        check=True,
                        timeout=30,
                    ).stdout
                    assert len(pixel) == 3
                    assert pixel[2] > pixel[0] + 100, pixel
            assert detail["channel"]["name"] == name, detail
            assert detail["files"], detail
            assert detail["streamingPlaylists"], detail
            hls = detail["streamingPlaylists"][0]
            expected_heights = [360] if audio_only else [360, 480]
            assert len(hls["files"]) == len(expected_heights), hls
            assert sorted(f["height"] for f in hls["files"]) == expected_heights, hls
            master = requests.get(hls["playlistUrl"], verify=False, timeout=30)
            master.raise_for_status()
            assert ("RESOLUTION=640x360" if audio_only else "RESOLUTION=640x480") in master.text
            if not audio_only:
                assert "RESOLUTION=480x360" in master.text
            variant_paths = [line for line in master.text.splitlines() if line.startswith("/media/hls/")]
            assert len(variant_paths) == len(expected_heights), master.text
            for path in variant_paths:
                variant = requests.get(
                    urljoin(master.url, path),
                    headers={"Origin": "https://peertube.local"},
                    verify=False,
                    timeout=30,
                )
                variant.raise_for_status()
                assert variant.headers["Access-Control-Allow-Origin"] == "*"
                assert "#EXT-X-BYTERANGE:" in variant.text
                segment_path = next(
                    line for line in variant.text.splitlines() if line.startswith("/media/")
                )
                segment = requests.get(
                    urljoin(master.url, segment_path),
                    headers={"Range": "bytes=0-63", "Origin": "https://peertube.local"},
                    verify=False,
                    timeout=30,
                )
                assert segment.status_code == 206, segment.text[:500]
                assert len(segment.content) == 64
                assert segment.headers["Access-Control-Allow-Origin"] == "*"
            low = next(f for f in hls["files"] if f["height"] == 360)
            low_bytes = requests.get(low["fileUrl"], verify=False, timeout=30)
            low_bytes.raise_for_status()
            with tempfile.TemporaryDirectory() as temp:
                lower = Path(temp) / "lower.mp4"
                lower.write_bytes(low_bytes.content)
                subprocess.run(
                    ["ffmpeg", "-v", "error", "-i", str(lower), "-t", "1", "-f", "null", "-"],
                    check=True,
                    timeout=30,
                )

            media_url = detail["files"][0]["fileUrl"]
            ranged = requests.get(
                media_url, headers={"Range": "bytes=0-63"}, verify=False, timeout=30
            )
            assert ranged.status_code == 206
            assert len(ranged.content) == 64
            with tempfile.TemporaryDirectory() as temp:
                received = Path(temp) / "received.mp4"
                full = requests.get(media_url, verify=False, timeout=30)
                full.raise_for_status()
                received.write_bytes(full.content)
                subprocess.run(
                    [
                        "ffmpeg",
                        "-v",
                        "error",
                        "-i",
                        str(received),
                        "-t",
                        "1",
                        "-f",
                        "null",
                        "-",
                    ],
                    check=True,
                    timeout=30,
                )
        if upload_mode in ("multipart", "audio-only"):
            with step("PeerTube's watch UI plays and seeks on desktop and mobile"):
                browser_check = (
                    Path(__file__).resolve().parents[2]
                    / "accessibility"
                    / "peertube-watch.mjs"
                )
                result = subprocess.run(
                    ["node", str(browser_check), f"{pt.base_url}/w/{remote['shortUUID']}"]
                    + (["audio-only"] if audio_only else []),
                    capture_output=True,
                    text=True,
                    timeout=120,
                    env={
                        **os.environ,
                        "PEERTUBE_E2E_USER": peertube.ROOT_USER,
                        "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
                    },
                )
                assert result.returncode == 0, result.stderr
                print(result.stdout, flush=True)
    finally:
        if published_id is not None:
            plamenu_api.delete_status(published_id)
        elif draft_url is not None:
            page = browser.get(f"{config.PLAMENU_URL}{draft_url}", timeout=30)
            if page.ok:
                browser.post(
                    f"{config.PLAMENU_URL}/web{draft_url}",
                    data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"},
                    allow_redirects=False,
                    timeout=30,
                )
        if subscribed:
            response = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30
            )
            assert response.status_code in (204, 404), response.text
