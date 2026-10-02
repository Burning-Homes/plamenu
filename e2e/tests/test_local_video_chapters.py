"""Local no-JS chapter authoring, PeerTube import and signed Update refresh."""

import base64
import hashlib
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


def _collection_url(status, chapters):
    """Match the public content revision to assert stale AP URLs are revoked."""
    digest = hashlib.sha256()
    for chapter in chapters:
        title = chapter["title"].encode()
        digest.update(chapter["start_seconds"].to_bytes(4, "big", signed=True))
        digest.update(len(title).to_bytes(8, "big"))
        digest.update(title)
    revision = base64.urlsafe_b64encode(digest.digest()[:12]).decode().rstrip("=")
    return f"{status['uri']}/chapters?v={revision}"


@pytest.mark.skipif(not peertube.reachable(), reason="peertube.local is not up")
@pytest.mark.federation(
    direction="outbound",
    peer="peertube",
    reverse_of="test_peertube_chapters_import_seek_and_update",
)
def test_local_chapters_draft_publish_update_remove(
    plamenu_api: Api, plamenu_user, cli
):
    pt = peertube.root()
    browser = requests.Session()
    browser.verify = False
    name = unique("chaplocal")[:30]
    title = f"Plamenu chaptered film {name}"
    channel = plamenu_api.post(
        "/api/v1/video_channels",
        username=name,
        display_name="Chaptered films",
    )
    draft_id = None
    published_id = None
    remote = None
    subscribed = False
    try:
        with step("PeerTube follows the channel and a no-JS upload reaches review"):
            pt.post(
                "/api/v1/users/me/subscriptions", uri=f"{name}@{config.PLAMENU_DOMAIN}"
            )
            subscribed = True
            login = browser.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": plamenu_user.email, "password": plamenu_user.password},
                allow_redirects=False,
                timeout=30,
            )
            assert login.status_code == 303
            csrf = _csrf(browser.get(f"{config.PLAMENU_URL}/videos", timeout=30).text)
            with tempfile.TemporaryDirectory() as directory:
                sample = peertube.make_sample_mp4(
                    Path(directory) / "chapters.mp4", seconds=20
                )
                uploaded = browser.post(
                    f"{config.PLAMENU_URL}/web/videos",
                    files=[
                        ("csrf", (None, csrf)),
                        ("channel", (None, str(channel["id"]))),
                        ("title", (None, title)),
                        ("description", (None, "Chaptered in Plamenu")),
                        ("language", (None, "en")),
                        ("visibility", (None, "public")),
                        ("file", ("chapters.mp4", sample.read_bytes(), "video/mp4")),
                        ("alt", (None, "Colour test pattern")),
                        ("op", (None, "review")),
                    ],
                    allow_redirects=False,
                    timeout=60,
                )
            assert uploaded.status_code == 303, uploaded.text[:500]
            draft_id = uploaded.headers["Location"].rsplit("/", 1)[1]
            wait_for(
                lambda: (
                    browser.get(
                        f"{config.PLAMENU_URL}/videos/drafts/{draft_id}/status",
                        timeout=30,
                    ).json()["phase"]
                    == "ready"
                ),
                desc="chapter draft ready",
                timeout=120,
            )

        with step(
            "draft chapter editor works without JS, validates and previews with JS"
        ):
            review_url = f"{config.PLAMENU_URL}/videos/drafts/{draft_id}/review"
            review = browser.get(review_url, timeout=30)
            review.raise_for_status()
            assert (
                "data-chapter-editor" in review.text
                and 'name="chapters"' in review.text
            )
            action = f"{config.PLAMENU_URL}/web/videos/drafts/{draft_id}/chapters"
            invalid = browser.post(
                action,
                data={
                    "csrf": _csrf(review.text),
                    "chapters": "0:00 Opening\n0:00 Duplicate",
                },
                allow_redirects=False,
                timeout=30,
            )
            assert invalid.status_code == 422 and "different start time" in invalid.text
            saved = browser.post(
                action,
                data={
                    "csrf": _csrf(review.text),
                    "chapters": "0:12 Final scene\n0:00 Opening scene\n0:06 Middle scene",
                },
                allow_redirects=False,
                timeout=30,
            )
            assert saved.status_code == 303, saved.text[:500]
            review = browser.get(review_url, timeout=30)
            assert (
                "0:00 Opening scene" in review.text
                and "0:12 Final scene" in review.text
            )
            screenshot = config.PLAMENU_DIR / "target/e2e/local-chapters-review.png"
            screenshot.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                [
                    "node",
                    "verify-local-chapter-editor.mjs",
                    review_url,
                    str(screenshot),
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                check=True,
                timeout=90,
                env={
                    **os.environ,
                    "PLAMENU_E2E_USER": plamenu_user.email,
                    "PLAMENU_E2E_PASSWORD": plamenu_user.password,
                },
            )

        with step(
            "published Video exposes a revisioned AP collection and PeerTube imports chapters"
        ):
            review = browser.get(review_url, timeout=30)
            published = browser.post(
                f"{config.PLAMENU_URL}/web/videos",
                files=[
                    ("csrf", (None, _csrf(review.text))),
                    ("channel", (None, str(channel["id"]))),
                    ("media_id", (None, draft_id)),
                    ("title", (None, title)),
                    ("description", (None, "Chaptered in Plamenu")),
                    ("language", (None, "en")),
                    ("visibility", (None, "public")),
                    ("op", (None, "publish")),
                ],
                allow_redirects=False,
                timeout=60,
            )
            assert published.status_code == 303, published.text[:500]
            published_id = published.headers["Location"].rsplit("/", 1)[1]
            status = plamenu_api.get_status(published_id)
            local_chapters = status["media_attachments"][0]["chapters"]
            assert [item["start_seconds"] for item in local_chapters] == [0, 6, 12]
            initial_url = _collection_url(status, local_chapters)
            collection = requests.get(
                initial_url,
                headers={"Accept": "application/activity+json"},
                verify=False,
                timeout=30,
            )
            assert collection.status_code == 200, collection.text[:500]
            assert collection.json()["id"] == initial_url
            assert [item["startOffset"] for item in collection.json()["hasPart"]] == [
                0,
                6,
                12,
            ]

            def imported():
                videos = pt.get(
                    "/api/v1/users/me/subscriptions/videos",
                    count=100,
                    sort="-publishedAt",
                )["data"]
                candidate = next(
                    (item for item in videos if item["name"] == title), None
                )
                if not candidate:
                    return None
                chapters = pt.get(f"/api/v1/videos/{candidate['uuid']}/chapters")[
                    "chapters"
                ]
                return (candidate, chapters) if len(chapters) == 3 else None

            remote, chapters = wait_for(
                imported, desc="PeerTube imports Plamenu chapters", timeout=120
            )
            assert [item["timecode"] for item in chapters] == [0, 6, 12]

        with step(
            "published owner edit federates; unrelated owner cannot change chapters"
        ):
            manage_url = f"{config.PLAMENU_URL}/videos/{draft_id}/chapters"
            library = browser.get(f"{config.PLAMENU_URL}/videos", timeout=30)
            library.raise_for_status()
            assert f'href="/videos/{draft_id}/manage"' in library.text
            workspace = browser.get(
                f"{config.PLAMENU_URL}/videos/{draft_id}/manage", timeout=30
            )
            workspace.raise_for_status()
            assert f'href="/videos/{draft_id}/chapters"' in workspace.text
            outsider = plamenu.User()
            cli.account_add(outsider)
            other = requests.Session()
            other.verify = False
            other_login = other.post(
                f"{config.PLAMENU_URL}/login",
                data={"email": outsider.email, "password": outsider.password},
                allow_redirects=False,
                timeout=30,
            )
            assert other_login.status_code == 303
            assert other.get(manage_url, timeout=30).status_code == 404
            other_csrf = _csrf(
                other.get(f"{config.PLAMENU_URL}/videos", timeout=30).text
            )
            denied = other.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/chapters",
                data={"csrf": other_csrf, "chapters": "0:00 Forged"},
                allow_redirects=False,
                timeout=30,
            )
            assert denied.status_code == 404
            denied_invalid = other.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/chapters",
                data={"csrf": other_csrf, "chapters": "0:60 Invalid"},
                allow_redirects=False,
                timeout=30,
            )
            assert denied_invalid.status_code == 404
            manager = browser.get(manage_url, timeout=30)
            manager.raise_for_status()
            assert "0:06 Middle scene" in manager.text
            screenshot = config.PLAMENU_DIR / "target/e2e/local-chapters-published.png"
            subprocess.run(
                [
                    "node",
                    "verify-local-chapter-editor.mjs",
                    manage_url,
                    str(screenshot),
                ],
                cwd=config.PLAMENU_DIR / "accessibility",
                check=True,
                timeout=90,
                env={
                    **os.environ,
                    "PLAMENU_E2E_USER": plamenu_user.email,
                    "PLAMENU_E2E_PASSWORD": plamenu_user.password,
                },
            )
            changed = browser.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/chapters",
                data={
                    "csrf": _csrf(manager.text),
                    "chapters": "0:00 New opening\n0:09 New ending",
                },
                allow_redirects=False,
                timeout=30,
            )
            assert changed.status_code == 303, changed.text[:500]

            def updated():
                chapters = pt.get(f"/api/v1/videos/{remote['uuid']}/chapters")[
                    "chapters"
                ]
                return (
                    chapters
                    if len(chapters) == 2 and chapters[1]["timecode"] == 9
                    else None
                )

            chapters = wait_for(
                updated, desc="PeerTube receives revised chapters", timeout=120
            )
            assert [item["title"] for item in chapters] == ["New opening", "New ending"]
            revised = plamenu_api.get_status(published_id)["media_attachments"][0][
                "chapters"
            ]
            assert [item["start_seconds"] for item in revised] == [0, 9]
            revised_url = _collection_url(status, revised)
            assert revised_url != initial_url
            assert (
                requests.get(
                    initial_url,
                    headers={"Accept": "application/activity+json"},
                    verify=False,
                    timeout=30,
                ).status_code
                == 404
            )
            assert (
                requests.get(
                    revised_url,
                    headers={"Accept": "application/activity+json"},
                    verify=False,
                    timeout=30,
                ).status_code
                == 200
            )
            manager = browser.get(manage_url, timeout=30)
            cleared = browser.post(
                f"{config.PLAMENU_URL}/web/videos/{draft_id}/chapters",
                data={"csrf": _csrf(manager.text), "chapters": ""},
                allow_redirects=False,
                timeout=30,
            )
            assert cleared.status_code == 303
            wait_for(
                lambda: (
                    not pt.get(f"/api/v1/videos/{remote['uuid']}/chapters")["chapters"]
                ),
                desc="PeerTube removes chapters",
                timeout=120,
            )
            assert (
                requests.get(
                    revised_url,
                    headers={"Accept": "application/activity+json"},
                    verify=False,
                    timeout=30,
                ).status_code
                == 404
            )
            empty_url = _collection_url(status, [])
            assert (
                requests.get(
                    empty_url,
                    headers={"Accept": "application/activity+json"},
                    verify=False,
                    timeout=30,
                ).json()["hasPart"]
                == []
            )
            plamenu_api.delete_status(published_id)
            published_id = None
            assert (
                requests.get(
                    empty_url,
                    headers={"Accept": "application/activity+json"},
                    verify=False,
                    timeout=30,
                ).status_code
                == 404
            )
    finally:
        if published_id is not None:
            plamenu_api.delete_status(published_id)
        elif draft_id is not None:
            page = browser.get(
                f"{config.PLAMENU_URL}/videos/drafts/{draft_id}", timeout=30
            )
            if page.ok:
                browser.post(
                    f"{config.PLAMENU_URL}/web/videos/drafts/{draft_id}",
                    data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"},
                    allow_redirects=False,
                    timeout=30,
                )
        if subscribed:
            response = pt.http.delete(
                f"{pt.base_url}/api/v1/users/me/subscriptions/{name}@{config.PLAMENU_DOMAIN}",
                timeout=30,
            )
            assert response.status_code in (204, 404), response.text
