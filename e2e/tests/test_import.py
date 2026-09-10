"""CSV data import end-to-end, driven through the first-party web UI.

A Plamenu user uploads a Mastodon-format `following_accounts.csv` naming a live
mastodon.local account, confirms the import, and the background import worker
applies it through the normal follow path — so the follow federates (signed
Follow into Mastodon, signed Accept back) exactly as an interactive follow
would. This is the one import path worth exercising against a real peer; the
parsing/coercion/worker mechanics are covered by the Rust integration tests.
"""

import re

import pytest
import requests
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for

CSRF_RE = re.compile(r'name="csrf" value="([^"]+)"')


def _csrf(html: str) -> str:
    match = CSRF_RE.search(html)
    assert match, "no CSRF token in the settings page"
    return match.group(1)


def _web_login(user) -> requests.Session:
    """A cookie session for Plamenu's first-party web UI."""
    session = requests.Session()
    session.verify = False
    resp = session.post(
        f"{config.PLAMENU_URL}/login",
        data={"email": user.email, "password": user.password},
        allow_redirects=False,
    )
    assert resp.status_code == 303, f"web login failed: {resp.status_code}"
    assert session.cookies, "web login set no session cookie"
    return session


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound server-lifecycle: importing a following CSV federates Follow activities to the listed accounts; no inbound counterpart.",
)
def test_import_following_csv_federates_follow(plamenu_user, plamenu_api, alice, db):
    """Covers: web login, multipart CSV upload + parse, confirm → scheduled,
    the import worker applying the row through FollowService, and the resulting
    Follow/Accept round-trip with mastodon.local."""
    web = _web_login(plamenu_user)

    with step("upload a following CSV naming a live mastodon.local account"):
        page = web.get(f"{config.PLAMENU_URL}/settings/export")
        csv = (
            "Account address,Show boosts,Notify on new posts,Languages\n"
            f"{config.ALICE},true,false,\n"
        )
        resp = web.post(
            f"{config.PLAMENU_URL}/web/settings/import",
            data={"csrf": _csrf(page.text), "type": "following", "mode": "merge"},
            files={"data": ("following_accounts.csv", csv, "text/csv")},
            allow_redirects=False,
        )
        assert resp.status_code == 303, f"upload failed: {resp.status_code}"
        import_path = resp.headers["location"]
        import_id = import_path.rsplit("/", 1)[-1]
        log(f"created unconfirmed import {import_id}")

    with step("confirm the import (schedules it for the worker)"):
        review = web.get(f"{config.PLAMENU_URL}{import_path}")
        assert "Confirm import" in review.text
        resp = web.post(
            f"{config.PLAMENU_URL}/web/settings/import/{import_id}/confirm",
            data={"csrf": _csrf(review.text)},
            allow_redirects=False,
        )
        assert resp.status_code == 303, f"confirm failed: {resp.status_code}"

    with step("the worker applies the row and the follow federates to Mastodon"):
        account = plamenu_api.resolve_account(config.ALICE)
        assert account, "Plamenu could not resolve alice"
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="plamenu's follow of alice to be accepted (imported + federated)",
        )
