"""Pleroma-side helpers: throwaway accounts (pleroma_ctl) and OAuth tokens.

The disposable Pleroma-family peer is provided by the external live-test
harness and federates over the same local CA as Mastodon and Plamenu. See
e2e/README.md for the public harness status. Account creation goes
through `pleroma_ctl` in the running container; tokens use the Mastodon-style
OAuth password grant, which Pleroma supports.
"""

import requests

from . import config, shell
from .api import Api, ApiError

OOB = "urn:ietf:wg:oauth:2.0:oob"

# Standing Pleroma account, created on demand (mirrors Mastodon's `alice`).
BOB_NICK = "bob"
BOB_PASS = "plrpass123"


def reachable() -> bool:
    """Whether the Pleroma test instance answers (tests skip when it doesn't)."""
    try:
        Api(config.PLEROMA_URL).get("/api/v1/instance")
        return True
    except (ApiError, requests.RequestException):
        return False


def _ctl(*args: str, timeout: float = 120) -> str:
    """Run `pleroma_ctl` inside the running web container.

    The peer runs Akkoma (a Pleroma fork), whose release lives at /opt/akkoma
    but keeps the `pleroma_ctl` binary name."""
    return shell.pleroma_compose(
        "exec", "-T", "pleroma", "/opt/akkoma/bin/pleroma_ctl", *args, timeout=timeout
    )


def ensure_user(nickname: str, password: str) -> None:
    """Create a Pleroma account; a no-op (tolerated error) if it already exists."""
    try:
        _ctl(
            "user",
            "new",
            nickname,
            f"{nickname}@{config.PLEROMA_DOMAIN}",
            "--password",
            password,
            "--assume-yes",
        )
    except RuntimeError:
        # Most likely "user already exists" — api_as() is the real validation.
        pass


def api_as(nickname: str, password: str, scopes: str = "read write follow") -> Api:
    """Authenticated Mastodon-API client for a Pleroma user (password grant)."""
    api = Api(config.PLEROMA_URL)
    app = api.post(
        "/api/v1/apps",
        client_name="plamenu-e2e",
        redirect_uris=OOB,
        scopes=scopes,
    )
    token = api.post(
        "/oauth/token",
        grant_type="password",
        username=nickname,
        password=password,
        client_id=app["client_id"],
        client_secret=app["client_secret"],
        scope=scopes,
    )["access_token"]
    return Api(config.PLEROMA_URL, token=token)


def bob() -> Api:
    """Authenticated client for the standing `bob` account, created on demand."""
    try:
        return api_as(BOB_NICK, BOB_PASS)
    except ApiError:
        ensure_user(BOB_NICK, BOB_PASS)
        return api_as(BOB_NICK, BOB_PASS)


def admin_bob() -> Api:
    """`bob` with the `admin` OAuth scope (and the admin flag set via
    pleroma_ctl — an idempotent boolean switch), for the emoji-pack API."""
    _ctl("user", "set", BOB_NICK, "--admin")
    return api_as(BOB_NICK, BOB_PASS, scopes="read write follow admin")


# Name of the throwaway emoji pack the suite manages on the Pleroma side.
EMOJI_PACK = "e2epack"


def ensure_custom_emoji(api_admin: Api, shortcode: str, png: bytes) -> None:
    """Create a local custom emoji on Pleroma (pack + file via the emoji-pack
    admin API), so `bob` can react with `:shortcode:`. Idempotent: an existing
    pack/file is tolerated. The emoji cache reloads on write, so the emoji is
    usable as soon as `/api/v1/custom_emojis` lists it."""
    # The disposable container ships without the static emoji dir, and the
    # pack API answers 500 (POSIX ENOENT) instead of creating it. The peer runs
    # Akkoma, whose static dir is /var/lib/akkoma and whose service user is
    # `akkoma`.
    shell.pleroma_compose(
        "exec",
        "-T",
        "-u",
        "root",
        "pleroma",
        "sh",
        "-c",
        "mkdir -p /var/lib/akkoma/static/emoji"
        " && chown -R akkoma /var/lib/akkoma/static",
    )
    r = api_admin.http.post(
        f"{api_admin.base_url}/api/v1/pleroma/emoji/pack",
        params={"name": EMOJI_PACK},
        timeout=30,
    )
    if not r.ok and "already exists" not in r.text:
        raise ApiError(f"emoji pack creation failed: {r.status_code} {r.text[:300]}")
    r = api_admin.http.post(
        f"{api_admin.base_url}/api/v1/pleroma/emoji/packs/files",
        params={"name": EMOJI_PACK},
        data={"shortcode": shortcode},
        files={"file": (f"{shortcode}.png", png, "image/png")},
        timeout=30,
    )
    if not r.ok and "already exists" not in r.text:
        raise ApiError(f"emoji file upload failed: {r.status_code} {r.text[:300]}")
    listed = [e["shortcode"] for e in api_admin.get("/api/v1/custom_emojis")]
    if shortcode not in listed:
        raise ApiError(f"emoji :{shortcode}: not in Pleroma's picker: {listed}")
