"""Helpers for the disposable Mitra peer at mitra.local."""

import json
import os
import subprocess
import uuid

import requests

from . import config
from .api import Api, ApiError

OOB = "urn:ietf:wg:oauth:2.0:oob"
ERIN_NICK = "erin"
ERIN_PASS = "mitra-erin-pass-123"
TOKEN_FILE = config.MITRA_DIR / ".erin-token"
SCOPES = "read write follow"


class MitraApi(Api):
    """Absorb Mitra client-API search differences from Mastodon.

    Mitra's v2 search intentionally ignores `resolve` and never resolves when
    `type=accounts`; remote account lookup lives at the v1 accounts search.
    URL status import uses untyped v2 search. Status edit and poll vote
    only accept JSON bodies (actix `web::Json`, unlike create's
    form-or-JSON extractor).
    """

    def edit_status(self, status_id: str, text: str) -> dict:
        return self._request(
            "PUT", f"/api/v1/statuses/{status_id}", json={"status": text}
        )

    def vote(self, poll_id: str, choices: list[int]) -> dict:
        return self._request(
            "POST", f"/api/v1/polls/{poll_id}/votes", json={"choices": choices}
        )

    def update_profile(
        self, *, display_name: str | None = None, note: str | None = None
    ) -> dict:
        """Edit erin's profile. Mitra's `update_credentials` is JSON-only
        (actix `web::Json`) and 415s the multipart body Api.update_profile
        sends, so override with a JSON PATCH."""
        payload: dict = {}
        if display_name is not None:
            payload["display_name"] = display_name
        if note is not None:
            payload["note"] = note
        return self._request(
            "PATCH", "/api/v1/accounts/update_credentials", json=payload
        )

    def resolve_account(self, acct: str) -> dict | None:
        rows = self.get("/api/v1/accounts/search", q=acct, resolve="true")
        return rows[0] if rows else None

    def resolve_status(self, uri: str) -> dict | None:
        rows = self.get("/api/v2/search", q=uri)["statuses"]
        return rows[0] if rows else None

    # Groups (FEP-1b12 hosting) — Mitra extensions, not Mastodon API. Any
    # user who can post may create one; the creator becomes its admin and
    # first follower, and the group Announces public activity to followers.
    # Create is JSON-only, like edit and vote.
    def create_group(self, name: str, description: str = "") -> dict:
        return self._request(
            "POST", "/api/v1/groups", json={"name": name, "description": description}
        )

    def followed_groups(self, only: str = "following") -> list:
        return self.get("/api/v1/groups/followed", filter=only)

    def delete_group(self, group_id: str) -> None:
        self.delete(f"/api/v1/groups/{group_id}")

    def post_to_group(self, group_id: str, text: str, **params) -> dict:
        return self.post_status(text, group_id=group_id, **params)


def reachable() -> bool:
    try:
        MitraApi(config.MITRA_URL).get("/api/v1/instance")
        return True
    except (ApiError, requests.RequestException):
        return False


def mint_token() -> str:
    """Mitra supports the Mastodon-compatible password grant."""
    api = MitraApi(config.MITRA_URL)
    app = api.post(
        "/api/v1/apps",
        client_name="plamenu-e2e",
        redirect_uris=OOB,
        scopes=SCOPES,
    )
    return api.post(
        "/oauth/token",
        grant_type="password",
        username=ERIN_NICK,
        password=ERIN_PASS,
        client_id=app["client_id"],
        client_secret=app["client_secret"],
        scope=SCOPES,
    )["access_token"]


def erin() -> Api:
    if TOKEN_FILE.is_file():
        api = MitraApi(config.MITRA_URL, token=TOKEN_FILE.read_text().strip())
        try:
            api.get("/api/v1/accounts/verify_credentials")
            return api
        except ApiError:
            pass
    token = mint_token()
    TOKEN_FILE.write_text(f"{token}\n")
    return MitraApi(config.MITRA_URL, token=token)


def like_activity(post_uri: str, actor_nick: str = ERIN_NICK) -> dict:
    """A minimal AS2 Like from a Mitra local user on `post_uri`, for
    `send_activity_rfc9421`. Mitra signs whatever activity it is handed as its
    `actor`, so this needs no server-side state on Mitra."""
    return {
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": f"{config.MITRA_URL}/objects/{uuid.uuid4()}",
        "type": "Like",
        "actor": f"{config.MITRA_URL}/users/{actor_nick}",
        "object": post_uri,
    }


def send_activity_rfc9421(activity: dict, recipient: str) -> str:
    """Deliver `activity` (signed as its `actor`, a Mitra local user) to
    `recipient`'s inbox with an **RFC 9421** signature, via Mitra's
    `send-activity --rfc9421` CLI.

    Mitra advertises + verifies RFC 9421 but *emits* draft-cavage by default
    (`rfc9421_enabled` is hardcoded false in its deliverer), so this CLI is the
    only way to make a real peer sign us with RFC 9421 — how the suite exercises
    inbound 9421 verification end-to-end. The recipient profile must already be
    known to Mitra (e.g. it has interacted with Mitra before). Returns the CLI's
    stdout (the delivery's HTTP status line)."""
    env = {
        **os.environ,
        "CONFIG_PATH": str(config.MITRA_DIR / "config.yaml"),
        "ENVIRONMENT": "production",
        "RUST_LOG": "warn",
    }
    proc = subprocess.run(
        [
            str(config.MITRA_SOURCE_DIR / "target" / "debug" / "mitra"),
            "send-activity",
            json.dumps(activity),
            "--recipient",
            recipient,
            "--rfc9421",
        ],
        capture_output=True,
        text=True,
        timeout=60,
        env=env,
        # Mitra's config uses a storage_dir relative to the source tree (the
        # dev script runs Mitra from there), so the CLI must share that cwd.
        cwd=config.MITRA_SOURCE_DIR,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            "mitra send-activity --rfc9421 failed: "
            f"{proc.stderr.strip() or proc.stdout.strip()}"
        )
    return proc.stdout.strip()


def known_status(api: Api, uri: str) -> dict | None:
    """Return Mitra's already-ingested copy without forcing dereference.

    Mitra's search cannot answer this: every URL query falls through to
    fetch-and-import when uncached, and `type=statuses` search is pure
    full-text, so a URI never matches. Erin follows the author in these
    tests, so a pushed Create lands on the home timeline — scan that.
    """
    return next((s for s in api.home_timeline(limit=40) if s["uri"] == uri), None)
