"""No-JS local caption authoring through review and PeerTube federation."""

import os
import re
import subprocess
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import config, peertube, plamenu, unique
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for


def _csrf(html):
    match = re.search(r'name="csrf" value="([^"]+)"', html)
    assert match
    return match.group(1)


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(direction="outbound", peer="peertube")
def test_local_caption_languages_review_publish_and_play(plamenu_api: Api, plamenu_user, cli):
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("captionlocal")[:30]
    title = f"Captioned Plamenu video {name}"
    channel = plamenu_api.post(
        "/api/v1/video_channels", username=name, display_name="Captioned films",
    )
    handle = f"{name}@{config.PLAMENU_DOMAIN}"
    draft_id = None
    published_id = None
    subscribed = False
    try:
        with step("PeerTube follows the author's channel and a no-JS upload creates a draft"):
            pt.post("/api/v1/users/me/subscriptions", uri=handle)
            subscribed = True
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False, timeout=30,
            )
            assert login.status_code == 303
            csrf = _csrf(browser.get(f"{config.PLAMENU_URL}/videos", timeout=30).text)
            with tempfile.TemporaryDirectory() as directory:
                sample = peertube.make_sample_mp4(Path(directory) / "captioned.mp4", seconds=12)
                uploaded = browser.post(
                    f"{config.PLAMENU_URL}/web/videos",
                    files=[
                        ("csrf", (None, csrf)),
                        ("channel", (None, str(channel["id"]))),
                        ("title", (None, title)),
                        ("description", (None, "A captioned film made in Plamenu.")),
                        ("language", (None, "en")),
                        ("visibility", (None, "public")),
                        ("file", ("captioned.mp4", sample.read_bytes(), "video/mp4")),
                        ("alt", (None, "Moving colour test pattern")),
                        ("transcript", (None, "An audible tone accompanies a moving pattern.")),
                        ("visual_audio", (None, "audio_description")),
                        ("captions", ("en.vtt", b"WEBVTT\n\n00:00:00.000 --> 00:00:10.000\nHello from Plamenu\n", "text/vtt")),
                        ("op", (None, "review")),
                    ],
                    allow_redirects=False, timeout=60,
                )
            assert uploaded.status_code == 303, uploaded.text[:500]
            draft_id = uploaded.headers["Location"].rsplit("/", 1)[1]
            wait_for(
                lambda: browser.get(f"{config.PLAMENU_URL}/videos/drafts/{draft_id}/status", timeout=30).json()["phase"] == "ready",
                desc="captioned draft ready", timeout=120,
            )

        with step("no-JS review adds, replaces, removes and re-adds French WebVTT"):
            review_url = f"{config.PLAMENU_URL}/videos/drafts/{draft_id}/review"
            action = f"{config.PLAMENU_URL}/web/videos/drafts/{draft_id}/captions"
            review = browser.get(review_url, timeout=30)
            review.raise_for_status()
            assert 'srclang="en"' in review.text
            assert 'data-caption-upload' in review.text

            def upload_french(text):
                return browser.post(
                    action,
                    files=[
                        ("csrf", (None, _csrf(browser.get(review_url, timeout=30).text))),
                        ("language", (None, "fr")),
                        ("caption", ("fr.vtt", f"WEBVTT\n\n00:00:00.000 --> 00:00:10.000\n{text}\n".encode(), "text/vtt")),
                    ],
                    allow_redirects=False, timeout=30,
                )

            rejected = browser.post(
                action,
                files=[
                    ("csrf", (None, _csrf(review.text))),
                    ("language", (None, "../fr")),
                    ("caption", ("fr.vtt", b"WEBVTT\n\n00:00:00.000 --> 00:00:03.000\nWrong\n", "text/vtt")),
                ],
                allow_redirects=False, timeout=30,
            )
            assert rejected.status_code == 422 and "valid caption language" in rejected.text

            added = upload_french("Bonjour initial")
            assert added.status_code == 303, added.text[:500]
            french_url = f"{config.PLAMENU_URL}/videos/drafts/{draft_id}/captions/fr.vtt"
            assert requests.get(french_url, verify=False, timeout=30).status_code == 404
            assert "Bonjour initial" in browser.get(french_url, timeout=30).text
            assert upload_french("Bonjour de Plamenu").status_code == 303
            assert "Bonjour de Plamenu" in browser.get(french_url, timeout=30).text
            review = browser.get(review_url, timeout=30)
            assert 'srclang="en"' in review.text and 'srclang="fr"' in review.text
            screenshot = config.PLAMENU_DIR / "target/e2e/local-captions-review.png"
            screenshot.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                ["node", "verify-local-caption-form.mjs", review_url, str(screenshot)],
                cwd=config.PLAMENU_DIR / "accessibility", check=True, timeout=60,
                env={**os.environ, "PLAMENU_E2E_USER": plamenu_user.email,
                     "PLAMENU_E2E_PASSWORD": plamenu_user.password},
            )
            removed = browser.post(
                f"{action}/fr/remove", data={"csrf": _csrf(review.text)},
                allow_redirects=False, timeout=30,
            )
            assert removed.status_code == 303
            assert browser.get(french_url, timeout=30).status_code == 404
            assert upload_french("Bonjour de Plamenu").status_code == 303

        with step("publication exposes both tracks locally and in signed ActivityPub"):
            review = browser.get(review_url, timeout=30)
            published = browser.post(
                f"{config.PLAMENU_URL}/web/videos",
                files=[
                    ("csrf", (None, _csrf(review.text))),
                    ("channel", (None, str(channel["id"]))),
                    ("media_id", (None, draft_id)),
                    ("title", (None, title)),
                    ("description", (None, "A captioned film made in Plamenu.")),
                    ("language", (None, "en")),
                    ("visibility", (None, "public")),
                    ("op", (None, "publish")),
                ],
                allow_redirects=False, timeout=60,
            )
            assert published.status_code == 303, published.text[:500]
            published_id = published.headers["Location"].rsplit("/", 1)[1]
            status = plamenu_api.get_status(published_id)
            assert status["media_attachments"][0]["audio_described"] is True
            assert status["media_attachments"][0]["transcript"] == "An audible tone accompanies a moving pattern."
            tracks = status["media_attachments"][0]["captions"]
            assert [track["language"] for track in tracks] == ["en", "fr"]
            assert all(browser.get(track["url"], timeout=30).status_code == 200 for track in tracks)
            assert status["uri"].endswith(f"/statuses/{published_id}")

        with step("PeerTube imports both languages and Plamenu plays the French cue"):
            def imported():
                videos = pt.get("/api/v1/users/me/subscriptions/videos", count=100, sort="-publishedAt")["data"]
                remote = next((item for item in videos if item["name"] == title), None)
                if not remote:
                    return None
                captions = pt.get(f"/api/v1/videos/{remote['uuid']}/captions")["data"]
                return (remote, captions) if len(captions) == 2 else None
            remote, captions = wait_for(imported, desc="PeerTube imports two Plamenu captions", timeout=120)
            assert {item["language"]["id"] for item in captions} == {"en", "fr"}
            subprocess.run(
                ["node", "verify-peertube-captions.mjs",
                 f"{config.PLAMENU_URL}/web/statuses/{published_id}", "Bonjour de Plamenu"],
                cwd=config.PLAMENU_DIR / "accessibility", check=True, timeout=120,
            )

        with step("published caption edits are reachable, owner-only and federated"):
            manage_url = f"{config.PLAMENU_URL}/videos/{draft_id}/captions"
            library = browser.get(f"{config.PLAMENU_URL}/videos", timeout=30)
            assert f'/videos/{draft_id}/captions' in library.text
            assert requests.get(manage_url, verify=False, allow_redirects=False, timeout=30).status_code != 200
            outsider = plamenu.User()
            cli.account_add(outsider)
            other_browser = requests.Session()
            other_browser.verify = False
            other_login = other_browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": outsider.email, "password": outsider.password},
                allow_redirects=False, timeout=30,
            )
            assert other_login.status_code == 303
            assert other_browser.get(manage_url, timeout=30).status_code == 404
            other_csrf = _csrf(other_browser.get(f"{config.PLAMENU_URL}/videos", timeout=30).text)
            denied = other_browser.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/captions",
                files=[
                    ("csrf", (None, other_csrf)),
                    ("language", (None, "es")),
                    ("caption", ("es.vtt", b"WEBVTT\n\n00:00:00.000 --> 00:00:03.000\nNo\n", "text/vtt")),
                ],
                allow_redirects=False, timeout=30,
            )
            assert denied.status_code == 404
            manager = browser.get(manage_url, timeout=30)
            manager.raise_for_status()
            assert 'srclang="en"' in manager.text and 'srclang="fr"' in manager.text
            screenshot = config.PLAMENU_DIR / "target/e2e/published-captions.png"
            subprocess.run(
                ["node", "verify-local-caption-form.mjs", manage_url, str(screenshot)],
                cwd=config.PLAMENU_DIR / "accessibility", check=True, timeout=60,
                env={**os.environ, "PLAMENU_E2E_USER": plamenu_user.email,
                     "PLAMENU_E2E_PASSWORD": plamenu_user.password},
            )

            def publish_caption(language, text):
                page = browser.get(manage_url, timeout=30)
                page.raise_for_status()
                return browser.post(
                    f"{config.PLAMENU_URL}/web/videos/{draft_id}/captions",
                    files=[
                        ("csrf", (None, _csrf(page.text))),
                        ("language", (None, language)),
                        ("caption", (f"{language}.vtt",
                                     f"WEBVTT\n\n00:00:00.000 --> 00:00:10.000\n{text}\n".encode(),
                                     "text/vtt")),
                    ],
                    allow_redirects=False, timeout=30,
                )

            assert publish_caption("es", "Hola de Plamenu").status_code == 303
            three = plamenu_api.get_status(published_id)["media_attachments"][0]["captions"]
            assert [track["language"] for track in three] == ["en", "es", "fr"]
            wait_for(
                lambda: (items if len(items := pt.get(
                    f"/api/v1/videos/{remote['uuid']}/captions")['data']) == 3 else None),
                desc="PeerTube receives caption-add Update", timeout=120,
            )
            old_french = next(track["url"] for track in three if track["language"] == "fr")
            assert publish_caption("fr", "Salut de Plamenu").status_code == 303
            three = plamenu_api.get_status(published_id)["media_attachments"][0]["captions"]
            new_french = next(track["url"] for track in three if track["language"] == "fr")
            assert old_french != new_french, "replacement must version its AP Link"
            def replaced_on_peer():
                item = next((entry for entry in pt.get(
                    f"/api/v1/videos/{remote['uuid']}/captions")['data']
                    if entry['language']['id'] == 'fr'), None)
                if not item:
                    return None
                response = pt.http.get(item['fileUrl'], timeout=30)
                return item if response.ok and "Salut de Plamenu" in response.text else None
            wait_for(replaced_on_peer, desc="PeerTube caches the replacement cue", timeout=120)
            assert "Salut de Plamenu" in browser.get(new_french, timeout=30).text

            manager = browser.get(manage_url, timeout=30)
            removed = browser.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/captions/en/remove",
                data={"csrf": _csrf(manager.text)}, allow_redirects=False, timeout=30,
            )
            assert removed.status_code == 303, removed.text[:500]
            assert browser.get(tracks[0]["url"], timeout=30).status_code == 404
            remaining = plamenu_api.get_status(published_id)["media_attachments"][0]["captions"]
            assert [track["language"] for track in remaining] == ["es", "fr"]
            def updated_languages():
                items = pt.get(f"/api/v1/videos/{remote['uuid']}/captions")['data']
                return items if {item['language']['id'] for item in items} == {'es', 'fr'} else None
            wait_for(
                updated_languages,
                desc="PeerTube receives caption-removal Update", timeout=120,
            )
            subprocess.run(
                ["node", "verify-peertube-captions.mjs",
                 f"{config.PLAMENU_URL}/web/statuses/{published_id}",
                 "Salut de Plamenu", '["es","fr"]'],
                cwd=config.PLAMENU_DIR / "accessibility", check=True, timeout=120,
            )
        with step("structured alternative edits commit before federation and retain caption languages"):
            edited = plamenu_api._request("PUT", f"/api/v1/statuses/{published_id}", json={
                "status": "A revised accessible film description.",
                "media_accessibility": [{"id": draft_id, "transcript": "Revised speech and visual information.",
                                      "audio_described": False, "visuals_conveyed_in_audio": True}],
                "media_attributes": [{"id": draft_id, "description": "Revised changing colour pattern"}]})
            attachment = edited["media_attachments"][0]
            assert attachment["transcript"] == "Revised speech and visual information."
            assert attachment["visuals_conveyed_in_audio"] is True and attachment["audio_described"] is False
            assert [track["language"] for track in attachment["captions"]] == ["es", "fr"]
            def description_updated():
                # PeerTube 8.3 embeds the complete description in video details;
                # the former separate /description endpoint no longer exists.
                response = pt.video(remote['uuid'])
                return response if "revised accessible film" in response.get("description", "") else None
            wait_for(description_updated, desc="PeerTube accepts the supported description alongside additive alternatives", timeout=120)
            assert {entry['language']['id'] for entry in pt.get(f"/api/v1/videos/{remote['uuid']}/captions")['data']} == {'es', 'fr'}
            transcript = browser.get(attachment["captions"][1]["transcript_url"], timeout=30)
            assert "Revised speech and visual information." in transcript.text
            assert "Revised changing colour pattern" in transcript.text

        with step("deleted video captions stop serving immediately"):
            old_urls = [track["url"] for track in remaining]
            plamenu_api.delete_status(published_id)
            published_id = None
            assert all(browser.get(url, timeout=30).status_code == 404 for url in old_urls)
    finally:
        if published_id is not None:
            plamenu_api.delete_status(published_id)
        elif draft_id is not None:
            page = browser.get(f"{config.PLAMENU_URL}/videos/drafts/{draft_id}", timeout=30)
            if page.ok:
                browser.post(
                    f"{config.PLAMENU_URL}/web/videos/drafts/{draft_id}",
                    data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"},
                    allow_redirects=False, timeout=30,
                )
        if subscribed:
            response = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{handle}", timeout=30,
            )
            assert response.status_code in (204, 404), response.text
