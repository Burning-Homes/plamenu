"""A real process crash must not lose or duplicate a native upload offset."""

import hashlib
import re

import requests
from plamenu_e2e import unique
from plamenu_e2e.steps import step


def _csrf(html: str) -> str:
    match = re.search(r'name="csrf" value="([^"]+)"', html)
    assert match, "video form has no CSRF token"
    return match.group(1)


def test_resumable_video_survives_process_crash(plamenu2, plamenu2_user):
    """Resume a 64 MiB source after SIGKILL without resetting the database."""
    api = plamenu2.login(plamenu2_user)
    channel = api.post(
        "/api/v1/video_channels",
        username=unique("resume")[:30],
        display_name="Crash recovery videos",
    )
    browser = requests.Session()
    browser.verify = False
    login = browser.post(
        f"{plamenu2.url}/login",
        data={"email": plamenu2_user.email, "password": plamenu2_user.password},
        allow_redirects=False,
        timeout=30,
    )
    assert login.status_code == 303, login.text[:500]
    page = browser.get(f"{plamenu2.url}/videos", timeout=30)
    page.raise_for_status()
    csrf = _csrf(page.text)
    # The content is deliberately not a decodable video: this test measures
    # transfer/recovery, not transcoding. Keep it above eight whole chunks.
    contents = b"\x07" * (64 * 1024 * 1024 + 123)
    edge = 64 * 1024
    fingerprint = hashlib.sha256(contents[:edge] + contents[-edge:]).hexdigest()
    with step("reserve the exact bytes and persist accessible metadata"):
        created = browser.post(
            f"{plamenu2.url}/web/videos/resumable",
            files=[
                ("csrf", (None, csrf)),
                ("channel", (None, str(channel["id"]))),
                ("title", (None, "Crash recovery film")),
                ("visibility", (None, "unlisted")),
                ("alt", (None, "Synthetic transfer test pattern")),
                ("source_size", (None, str(len(contents)))),
                ("source_fingerprint", (None, fingerprint)),
            ],
            timeout=30,
        )
        assert created.status_code == 200, created.text[:500]
        draft_id = created.json()["id"]
        assert isinstance(draft_id, str) and draft_id.isdecimal()
    source = f"{plamenu2.url}/web/videos/drafts/{draft_id}/source"
    chunk_size = 8 * 1024 * 1024
    with step("commit one bounded chunk, then kill the server"):
        first = browser.patch(
            source,
            headers={"x-csrf-token": csrf, "x-upload-offset": "0"},
            data=contents[:chunk_size],
            timeout=60,
        )
        assert first.status_code == 200, first.text[:500]
        assert first.json()["offset"] == chunk_size
        plamenu2.restart_process_after_crash()
    with step("the restarted process resumes from the committed offset"):
        status = browser.get(source, timeout=30)
        status.raise_for_status()
        assert status.json()["offset"] == chunk_size
        assert status.json()["total"] == len(contents)
        assert status.json()["fingerprint"] == fingerprint
        for offset in range(chunk_size, len(contents), chunk_size):
            chunk = contents[offset : offset + chunk_size]
            response = browser.patch(
                source,
                headers={"x-csrf-token": csrf, "x-upload-offset": str(offset)},
                data=chunk,
                timeout=60,
            )
            assert response.status_code == 200, response.text[:500]
            assert response.json()["offset"] == offset + len(chunk)
        assert (
            browser.get(f"{plamenu2.url}/media/{draft_id}.orig", timeout=30).status_code
            == 404
        )
        finished = browser.post(
            f"{source}/finish", headers={"x-csrf-token": csrf}, timeout=30
        )
        assert finished.status_code == 200, finished.text[:500]
        phase = browser.get(
            f"{plamenu2.url}/videos/drafts/{draft_id}/status", timeout=30
        )
        phase.raise_for_status()
        assert phase.json()["phase"] != "receiving"
    with step("cancel the unpublished test draft and retain private-source gating"):
        page = browser.get(f"{plamenu2.url}/videos/drafts/{draft_id}", timeout=30)
        page.raise_for_status()
        cancelled = browser.post(
            f"{plamenu2.url}/web/videos/drafts/{draft_id}",
            data={"csrf": _csrf(page.text), "op": "cancel", "confirm": "yes"},
            allow_redirects=False,
            timeout=30,
        )
        assert cancelled.status_code == 303, cancelled.text[:500]
        assert (
            browser.get(f"{plamenu2.url}/media/{draft_id}.orig", timeout=30).status_code
            == 404
        )
