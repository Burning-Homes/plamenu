"""Actual native film replacement delivered as Update, not a new PeerTube Video."""

import hashlib
import os
import re
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import urljoin

import pytest
import requests
from plamenu_e2e import apsign, config, peertube, unique
from plamenu_e2e.steps import step, wait_for


def csrf(html):
    return re.search(r'name="csrf" value="([^"]+)"', html)[1]


def verify_peers(local_url, remote_url, stem):
    result = subprocess.run(
        ["node", "verify-native-p2p.mjs", local_url, remote_url, "--expect-blue"],
        cwd=config.PLAMENU_DIR / "accessibility",
        capture_output=True,
        text=True,
        timeout=150,
        env={
            **os.environ,
            "PEERTUBE_E2E_USER": peertube.ROOT_USER,
            "PEERTUBE_E2E_PASSWORD": peertube.ROOT_PASS,
        },
        check=False,
    )
    evidence = config.PLAMENU_DIR / f"target/e2e/{stem}-p2p.log"
    evidence.parent.mkdir(parents=True, exist_ok=True)
    evidence.write_text(result.stdout + result.stderr)
    print(result.stdout)
    if result.returncode:
        print(result.stderr)
    result.check_returncode()


def prepare(browser, channel, movie, title, transcript):
    page = browser.get(config.PLAMENU_URL + "/videos", timeout=30)
    with movie.open("rb") as source:
        fields = {
            "csrf": csrf(page.text),
            "channel": channel["id"],
            "title": title,
            "description": "Private replacement draft",
            "language": "en",
            "visibility": "public",
            "alt": "A blue replacement film."
            if "Replacement" in title
            else "A moving test pattern.",
            "transcript": transcript,
            "op": "upload",
        }
        result = browser.post(
            config.PLAMENU_URL + "/web/videos",
            files=[
                *((key, (None, str(value))) for key, value in fields.items()),
                ("file", (movie.name, source, "video/mp4")),
            ],
            allow_redirects=False,
            timeout=90,
        )
    assert result.status_code == 303, result.text[:500]
    draft = urljoin(config.PLAMENU_URL, result.headers["Location"])

    def phase():
        response = browser.get(draft + "/status", timeout=30)
        value = response.json()
        return value if value["phase"] in ("ready", "failed") else None

    assert (
        wait_for(phase, desc="private replacement native encoding", timeout=180)[
            "phase"
        ]
        == "ready"
    )
    review = browser.get(draft + "/review", timeout=30)
    caption = browser.post(
        config.PLAMENU_URL + f"/web/videos/drafts/{draft.rsplit('/', 1)[1]}/captions",
        files=[
            ("csrf", (None, csrf(review.text))),
            ("language", (None, "en")),
            (
                "caption",
                (
                    "captions.vtt",
                    f"WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[{title}]\n".encode(),
                    "text/vtt",
                ),
            ),
        ],
        allow_redirects=False,
        timeout=30,
    )
    assert caption.status_code == 303, caption.text[:500]
    return draft


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="Plamenu creator replacement preserves the remote PeerTube Video identity",
)
def test_native_replacement_preserves_social_media_and_peertube_identity(
    plamenu_api, plamenu_user, db
):
    api = plamenu_api
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("replace")[:30]
    channel = api.post(
        "/api/v1/video_channels", username=name, display_name="Replacement films"
    )
    posted = reply = None
    drafts = []
    subscribed = False
    preferences = api.get("/api/v1/video_viewer_preferences")
    try:
        browser.post(
            config.PLAMENU_URL + "/login",
            data={"email": plamenu_user.email, "password": plamenu_user.password},
            timeout=30,
        )
        with tempfile.TemporaryDirectory() as temporary:
            # Multiple fragments beyond the production 15-second urgent HTTP
            # window are necessary to exercise actual stale peer requests.
            original = peertube.make_sample_mp4(
                Path(temporary) / "original.mp4", seconds=30, size="640x480"
            )
            replacement = Path(temporary) / "replacement.mp4"
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=blue:size=640x480:rate=10",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=880",
                    "-t",
                    "36",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-movflags",
                    "+faststart",
                    str(replacement),
                ],
                check=True,
                timeout=30,
            )
            with step("publish the original film and receive it in PeerTube"):
                old = prepare(
                    browser, channel, original, f"Film {name}", "Original steady tone."
                )
                drafts.append(old)
                media_id = old.rsplit("/", 1)[1]
                pt.post(
                    "/api/v1/users/me/subscriptions",
                    uri=f"{name}@{config.PLAMENU_DOMAIN}",
                )
                subscribed = True
                film = api.post(
                    "/api/v1/videos",
                    channel_id=channel["id"],
                    media_id=media_id,
                    title=f"Film {name}",
                    description="Original public description",
                    visibility="public",
                    language="en",
                )
                posted = film["id"]
                old_media = film["media_attachments"][0]
                remote = wait_for(
                    lambda: next(
                        (
                            v
                            for v in pt.get(
                                "/api/v1/users/me/subscriptions/videos",
                                count=100,
                                sort="-publishedAt",
                            )["data"]
                            if v["name"] == f"Film {name}"
                        ),
                        None,
                    ),
                    desc="PeerTube receives original native film",
                )
                remote = pt.video(remote["uuid"])
                reply = api.post_status(
                    "The replacement must keep this reply", in_reply_to_id=posted
                )["id"]
                api.post(f"/api/v1/statuses/{posted}/favourite")
                api.post(f"/api/v1/statuses/{posted}/bookmark")
                api.put("/api/v1/video_viewer_preferences", history_enabled="true")
                assert api.put(
                    f"/api/v1/video_history/{media_id}", position_seconds="2"
                )["saved"]
                old_master = old_media["hls"]["master"]
                old_source = db._value(
                    "SELECT source_file FROM video_drafts WHERE media_id=%s",
                    int(media_id),
                )
            with step(
                "process and review a private replacement while the old film still serves"
            ):
                new = prepare(
                    browser,
                    channel,
                    replacement,
                    f"Replacement {name}",
                    "A new 880 Hz steady tone.",
                )
                drafts.append(new)
                candidate = new.rsplit("/", 1)[1]
                preview = browser.get(new + "/preview/video", timeout=30)
                assert preview.status_code == 200
                new_digest = hashlib.sha256(preview.content).hexdigest()
                assert (
                    requests.get(old_media["url"], verify=False, timeout=30).status_code
                    == 200
                )
                assert (
                    requests.get(
                        new + "/preview/video", verify=False, timeout=30
                    ).status_code
                    == 404
                )
                manager = browser.get(
                    config.PLAMENU_URL
                    + f"/videos/{media_id}/edit?candidate={candidate}",
                    timeout=30,
                )
                assert (
                    manager.status_code == 200
                    and "Private replacement preview" in manager.text
                )
                denied = browser.post(
                    config.PLAMENU_URL + f"/web/videos/{media_id}/edit",
                    data={
                        "csrf": csrf(manager.text),
                        "candidate": candidate,
                        "revision": "0",
                    },
                    allow_redirects=False,
                    timeout=30,
                )
                assert denied.status_code == 422
            with step(
                "apply only after confirmation; maintain status, media, channel, author and remote UUID"
            ):
                subprocess.run(
                    ["node", "verify-video-replacement.mjs", film["url"], candidate],
                    cwd=config.PLAMENU_DIR / "accessibility",
                    check=True,
                    timeout=150,
                    env={
                        **os.environ,
                        "AP09_REPLACEMENT_AUTH": api.http.headers["Authorization"],
                    },
                )
                changed = api.get_status(posted)
                assert changed["id"] == film["id"] and changed["url"] == film["url"]
                assert changed["account"]["id"] == film["account"]["id"]
                assert (
                    changed["title"] == film["title"]
                    and changed["content"] == film["content"]
                )
                assert (
                    changed["favourited"]
                    and changed["bookmarked"]
                    and changed["replies_count"] == 1
                )
                assert api.get_status(reply)["in_reply_to_id"] == posted
                current = changed["media_attachments"][0]
                object_response = apsign.signed_ap_get(changed["uri"])
                object_response.raise_for_status()
                # Public interoperability evidence; no token, private draft or
                # credentials are included in the ActivityPub Video document.
                evidence = (
                    config.PLAMENU_DIR
                    / "target/e2e/ap09-video-edit-current-object.json"
                )
                evidence.parent.mkdir(parents=True, exist_ok=True)
                evidence.write_text(object_response.text)
                assert object_response.json()["icon"][0]["mediaType"] == "image/jpeg"
                assert current["id"] == media_id and current["content_revision"] == 1
                assert (
                    "rev=1" in current["hls"]["master"]
                    and current["hls"]["master"] != old_master
                )
                assert current["transcript"] == "A new 880 Hz steady tone."
                assert (
                    hashlib.sha256(
                        requests.get(current["url"], verify=False, timeout=30).content
                    ).hexdigest()
                    == new_digest
                )
                assert (
                    requests.get(old_media["url"], verify=False, timeout=30).status_code
                    == 404
                )
                assert (
                    requests.get(
                        current["hls"]["master"].replace("rev=1", "rev=0"),
                        verify=False,
                        timeout=30,
                    ).status_code
                    == 404
                )
                assert (
                    db._value(
                        "SELECT source_file FROM video_drafts WHERE media_id=%s",
                        int(media_id),
                    )
                    != old_source
                )
                assert browser.get(new + "/review", timeout=30).status_code == 404
                assert api.get("/api/v1/video_history")["data"] == []
                assert not api.put(
                    f"/api/v1/video_history/{media_id}",
                    position_seconds="2",
                    content_revision="0",
                )["saved"]
                assert api.put(
                    f"/api/v1/video_history/{media_id}",
                    position_seconds="3",
                    content_revision="1",
                )["saved"]

                def updated_remote():
                    detail = pt.video(remote["uuid"])
                    links = detail.get("streamingPlaylists", [])
                    return (
                        detail if links and "rev=1" in links[0]["playlistUrl"] else None
                    )

                updated = wait_for(
                    updated_remote,
                    desc="PeerTube accepts stable-UUID native file replacement",
                )
                assert (
                    updated["uuid"] == remote["uuid"]
                    and updated["url"] == remote["url"]
                )
                assert updated["duration"] == 36
                tracks = pt.get(f"/api/v1/videos/{remote['uuid']}/captions")
                assert (
                    tracks["total"] == 1 and tracks["data"][0]["language"]["id"] == "en"
                )
                checksum = requests.get(
                    updated["streamingPlaylists"][0]["segmentsSha256Url"],
                    verify=False,
                    timeout=30,
                )
                assert checksum.status_code == 200 and checksum.json()
                # Both actual first-party players exchange validated replacement
                # bytes using the preserved UUID and new integrity metadata.
                verify_peers(
                    changed["url"],
                    f"{pt.base_url}/w/{remote['uuid']}",
                    f"ap09-video-replacement-{name}",
                )
    finally:
        api._request(
            "PUT",
            "/api/v1/video_viewer_preferences",
            json={"history_enabled": preferences["history_enabled"]},
        )
        if reply:
            api.delete_status(reply)
        if posted:
            api.delete_status(posted)
        for draft in drafts:
            page = browser.get(draft, timeout=30)
            if page.status_code == 200:
                browser.post(
                    config.PLAMENU_URL
                    + f"/web/videos/drafts/{draft.rsplit('/', 1)[1]}",
                    data={"csrf": csrf(page.text), "op": "cancel", "confirm": "yes"},
                    timeout=30,
                )
        if subscribed:
            response = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}",
                timeout=30,
            )
            assert response.status_code in (204, 404)
        api.post(
            f"/api/v1/video_channels/{channel['id']}",
            operation="delete",
            confirm=channel["username"],
        )


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    one_way_reason="Native retained-source edits update the original federated Video",
)
def test_native_reprocess_and_trim_preserve_peertube_identity_and_play_new_timeline(
    plamenu_api, plamenu_user, db
):
    api, pt = plamenu_api, peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("sourceedit")[:30]
    channel = api.post(
        "/api/v1/video_channels", username=name, display_name="Source edits"
    )
    posted = None
    drafts = []
    subscribed = False
    preferences = api.get("/api/v1/video_viewer_preferences")
    try:
        browser.post(
            config.PLAMENU_URL + "/login",
            data={"email": plamenu_user.email, "password": plamenu_user.password},
            timeout=30,
        )
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "colour-sections.mp4"
            subprocess.run(
                [
                    "ffmpeg",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:size=640x480:rate=10:duration=12",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=blue:size=640x480:rate=10:duration=60",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=green:size=640x480:rate=10:duration=12",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=880",
                    "-filter_complex",
                    "[0:v][1:v][2:v]concat=n=3:v=1:a=0[v]",
                    "-map",
                    "[v]",
                    "-map",
                    "3:a:0",
                    "-t",
                    "84",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-movflags",
                    "+faststart",
                    str(source),
                ],
                check=True,
                timeout=45,
            )
            with step("publish retained original and receive it in actual PeerTube"):
                draft = prepare(
                    browser,
                    channel,
                    source,
                    f"Source {name}",
                    "Red, blue and green sections with a steady tone.",
                )
                drafts.append(draft)
                media_id = draft.rsplit("/", 1)[1]
                review = browser.get(draft + "/review", timeout=30)
                for language in ("en", "fr"):
                    caption = browser.post(
                        config.PLAMENU_URL + f"/web/videos/drafts/{media_id}/captions",
                        files=[
                            ("csrf", (None, csrf(review.text))),
                            ("language", (None, language)),
                            (
                                "caption",
                                (
                                    "sections.vtt",
                                    f"WEBVTT\n\n00:00:12.000 --> 00:01:12.000\n[Blue section {language}]\n".encode(),
                                    "text/vtt",
                                ),
                            ),
                        ],
                        allow_redirects=False,
                        timeout=30,
                    )
                    assert caption.status_code == 303, caption.text[:500]
                pt.post(
                    "/api/v1/users/me/subscriptions",
                    uri=f"{name}@{config.PLAMENU_DOMAIN}",
                )
                subscribed = True
                film = api.post(
                    "/api/v1/videos",
                    channel_id=channel["id"],
                    media_id=media_id,
                    title=f"Source {name}",
                    description="Stable public post",
                    visibility="public",
                    language="en",
                )
                posted = film["id"]
                remote = wait_for(
                    lambda: next(
                        (
                            v
                            for v in pt.get(
                                "/api/v1/users/me/subscriptions/videos",
                                count=100,
                                sort="-publishedAt",
                            )["data"]
                            if v["name"] == f"Source {name}"
                        ),
                        None,
                    ),
                    desc="PeerTube receives retained-source film",
                )
                remote = pt.video(remote["uuid"])
                api.put("/api/v1/video_viewer_preferences", history_enabled="true")
                assert api.put(
                    f"/api/v1/video_history/{media_id}", position_seconds="25"
                )["saved"]

            def prepared(operation, revision, **cut):
                result = api.http.post(
                    f"{api.base_url}/api/v1/videos/{media_id}/edits",
                    json={"operation": operation, "revision": revision, **cut},
                    timeout=30,
                )
                assert result.status_code == 202, result.text[:500]
                candidate = result.json()["media_id"]
                drafts.append(config.PLAMENU_URL + f"/videos/drafts/{candidate}")

                def ready():
                    status = api.get(f"/api/v1/video_edits/{candidate}")
                    return (
                        status
                        if status["phase"] in ("ready", "failed", "expired")
                        else None
                    )

                assert (
                    wait_for(ready, desc=f"private native {operation}", timeout=180)[
                        "phase"
                    ]
                    == "ready"
                )
                return candidate

            def peer_revision(revision):
                def updated():
                    value = pt.video(remote["uuid"])
                    links = value.get("streamingPlaylists", [])
                    return (
                        value
                        if links and f"rev={revision}" in links[0]["playlistUrl"]
                        else None
                    )

                value = wait_for(
                    updated, desc=f"PeerTube accepts native edit revision {revision}"
                )
                assert value["uuid"] == remote["uuid"] and value["url"] == remote["url"]
                return value

            with step(
                "reprocess through durable copy/encoder, retaining identity and viewing position"
            ):
                candidate = prepared("reprocess", 0)
                preview = browser.get(
                    config.PLAMENU_URL + f"/videos/drafts/{candidate}/preview/video",
                    timeout=30,
                )
                assert preview.status_code == 200
                assert (
                    requests.get(
                        config.PLAMENU_URL
                        + f"/videos/drafts/{candidate}/preview/video",
                        verify=False,
                        timeout=30,
                    ).status_code
                    == 404
                )
                assert (
                    api.get_status(posted)["media_attachments"][0]["content_revision"]
                    == 0
                )
                applied = api.http.put(
                    f"{api.base_url}/api/v1/videos/{media_id}/content",
                    json={
                        "candidate_media_id": candidate,
                        "revision": 0,
                        "confirm": True,
                    },
                    timeout=30,
                )
                assert applied.status_code == 200, applied.text[:500]
                assert peer_revision(1)["duration"] == 84
                assert (
                    api.get("/api/v1/video_history")["data"][0]["position_seconds"]
                    == 25
                )
                assert not api.put(
                    f"/api/v1/video_history/{media_id}",
                    position_seconds="26",
                    content_revision="0",
                )["saved"]

            with step(
                "trim only blue footage, review alternatives, receive same-UUID Update and new captions"
            ):
                candidate = prepared("trim", 1, start_ms=20_000, end_ms=56_000)
                review = browser.get(
                    config.PLAMENU_URL + f"/videos/drafts/{candidate}/review",
                    timeout=30,
                )
                assert (
                    review.status_code == 200
                    and "Save alternatives and review edit" in review.text
                )
                assert (
                    'value="publish"' not in review.text
                    and 'value="schedule"' not in review.text
                )
                alternatives = browser.post(
                    config.PLAMENU_URL + f"/web/videos/drafts/{candidate}/alternatives",
                    data={
                        "csrf": csrf(review.text),
                        "alt": "Blue section only",
                        "transcript": "A steady 880 Hz tone in the selected section.",
                        "visual_audio": "",
                    },
                    allow_redirects=False,
                    timeout=30,
                )
                assert alternatives.status_code == 303, alternatives.text[:500]
                for language in ("en", "fr"):
                    captions = browser.get(
                        config.PLAMENU_URL
                        + f"/videos/drafts/{candidate}/captions/{language}.vtt",
                        timeout=30,
                    )
                    assert (
                        captions.status_code == 200
                        and "00:00:00.000 --> 00:00:36.000" in captions.text
                    )
                applied = api.http.put(
                    f"{api.base_url}/api/v1/videos/{media_id}/content",
                    json={
                        "candidate_media_id": candidate,
                        "revision": 1,
                        "confirm": True,
                    },
                    timeout=30,
                )
                assert applied.status_code == 200, applied.text[:500]
                changed = api.get_status(posted)
                assert (
                    changed["id"] == film["id"]
                    and changed["url"] == film["url"]
                    and changed["title"] == film["title"]
                )
                current = changed["media_attachments"][0]
                assert (
                    current["id"] == media_id
                    and current["content_revision"] == 2
                    and current["description"] == "Blue section only"
                )
                assert api.get("/api/v1/video_history")["data"] == []
                updated = peer_revision(2)
                assert updated["duration"] == 36
                tracks = pt.get(f"/api/v1/videos/{remote['uuid']}/captions")
                assert {track["language"]["id"] for track in tracks["data"]} == {
                    "en",
                    "fr",
                }
                retained = db._value(
                    "SELECT clip_start_ms::text || ':' || clip_end_ms::text FROM video_drafts WHERE media_id=%s",
                    int(media_id),
                )
                assert retained == "20000:56000"
                response = apsign.signed_ap_get(changed["uri"])
                response.raise_for_status()
                evidence = (
                    config.PLAMENU_DIR
                    / "target/e2e/ap09-video-source-edit-current-object.json"
                )
                evidence.parent.mkdir(parents=True, exist_ok=True)
                evidence.write_text(response.text)
                verify_peers(
                    changed["url"],
                    f"{pt.base_url}/w/{remote['uuid']}",
                    f"ap09-video-source-edit-{name}",
                )
    finally:
        api._request(
            "PUT",
            "/api/v1/video_viewer_preferences",
            json={"history_enabled": preferences["history_enabled"]},
        )
        if posted:
            api.delete_status(posted)
        for draft in drafts:
            page = browser.get(draft, timeout=30)
            if page.status_code == 200:
                browser.post(
                    config.PLAMENU_URL
                    + f"/web/videos/drafts/{draft.rsplit('/', 1)[1]}",
                    data={"csrf": csrf(page.text), "op": "cancel", "confirm": "yes"},
                    timeout=30,
                )
        if subscribed:
            result = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}",
                timeout=30,
            )
            assert result.status_code in (204, 404)
        api.post(
            f"/api/v1/video_channels/{channel['id']}",
            operation="delete",
            confirm=channel["username"],
        )
