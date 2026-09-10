"""Upstream-Pleroma-side helpers: throwaway accounts (pleroma_ctl) and tokens.

`plup` is a SEPARATE peer from the Akkoma `pleroma` one (see pleroma.py): it
runs real upstream Pleroma 2.10.2 on its own domain (https://plup.local) so the
suite tests against the actual upstream software, whose HTTP-signature parser
does NOT tolerate RFC 9421 the way Akkoma does. This peer is the regression
guard for the RFC 9421 black-hole bug (plamenu/RFC9421_PLEROMA_FIX_PLAN.md).

Account creation goes through `pleroma_ctl` in the running container (note the
upstream path /opt/pleroma, not Akkoma's /opt/akkoma); tokens use the
Mastodon-style OAuth password grant. The API surface is the same Pleroma-family
one as Akkoma, so the emoji-pack helpers port directly.
"""

import json

import requests

from . import config, shell
from .api import Api, ApiError

OOB = "urn:ietf:wg:oauth:2.0:oob"

# Standing upstream-Pleroma admin account, created on demand.
MARI_NICK = "mari"
MARI_PASS = "plup-testpass-123"


def reachable() -> bool:
    """Whether the upstream-Pleroma instance answers (tests skip when it doesn't)."""
    try:
        Api(config.PLUP_URL).get("/api/v1/instance")
        return True
    except (ApiError, requests.RequestException):
        return False


def _ctl(*args: str, timeout: float = 120) -> str:
    """Run `pleroma_ctl` inside the running web container.

    Upstream Pleroma's release lives at /opt/pleroma (NOT Akkoma's
    /opt/akkoma)."""
    return shell.plup_compose(
        "exec", "-T", "pleroma", "/opt/pleroma/bin/pleroma_ctl", *args, timeout=timeout
    )


def ensure_user(nickname: str, password: str, admin: bool = False) -> None:
    """Create an upstream-Pleroma account; a no-op if it already exists."""
    try:
        args = [
            "user",
            "new",
            nickname,
            f"{nickname}@{config.PLUP_DOMAIN}",
            "--password",
            password,
        ]
        if admin:
            args.append("--admin")
        args.append("--assume-yes")
        _ctl(*args)
    except RuntimeError:
        # Most likely "user already exists" — api_as() is the real validation.
        pass


def api_as(nickname: str, password: str, scopes: str = "read write follow") -> Api:
    """Authenticated Mastodon-API client for an upstream-Pleroma user (password grant)."""
    api = Api(config.PLUP_URL)
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
    return Api(config.PLUP_URL, token=token)


def mari() -> Api:
    """Authenticated client for the standing `mari` account, created on demand."""
    try:
        return api_as(MARI_NICK, MARI_PASS)
    except ApiError:
        ensure_user(MARI_NICK, MARI_PASS, admin=True)
        return api_as(MARI_NICK, MARI_PASS)


def admin_mari() -> Api:
    """`mari` with the `admin` OAuth scope (and the admin flag set via
    pleroma_ctl — an idempotent boolean switch), for the emoji-pack API."""
    _ctl("user", "set", MARI_NICK, "--admin")
    return api_as(MARI_NICK, MARI_PASS, scopes="read write follow admin")


def refresh_follow_information(actor_uri: str) -> None:
    """Make the running Pleroma node refresh one remote actor's collection
    metadata.

    Pleroma's normal ``make_user_from_ap_id`` path currently puts the fetched
    metadata under a legacy ``info`` map that ``remote_user_changeset`` no
    longer persists. Calling the public ``User.fetch_follow_information/1``
    function exercises the same real signed collection fetches and stores the
    resulting counters/privacy flags, making authorized-fetch E2E assertions
    deterministic.
    """
    uri = json.dumps(actor_uri)
    expression = (
        f"user = Pleroma.User.get_cached_by_ap_id({uri}); "
        "case Pleroma.User.fetch_follow_information(user) do "
        "{:ok, _user} -> :ok; error -> raise inspect(error) end"
    )
    shell.plup_compose(
        "exec",
        "-T",
        "pleroma",
        "/opt/pleroma/bin/pleroma",
        "rpc",
        expression,
    )


# Name of the throwaway emoji pack the suite manages on the upstream-Pleroma side.
EMOJI_PACK = "e2epack"


def ensure_custom_emoji(api_admin: Api, shortcode: str, png: bytes) -> None:
    """Create a local custom emoji on upstream Pleroma (pack + file via the
    emoji-pack admin API), so `mari` can react with `:shortcode:`. Idempotent.
    The emoji cache reloads on write, so the emoji is usable as soon as
    `/api/v1/custom_emojis` lists it."""
    # Ensure the static emoji dir exists (upstream Pleroma's static_dir is
    # /var/lib/pleroma/static, owned by the `pleroma` service user), otherwise
    # the pack API answers 500 (POSIX ENOENT) instead of creating it.
    shell.plup_compose(
        "exec",
        "-T",
        "pleroma",
        "sh",
        "-c",
        "mkdir -p /var/lib/pleroma/static/emoji",
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
        raise ApiError(
            f"emoji :{shortcode}: not in upstream Pleroma's picker: {listed}"
        )
