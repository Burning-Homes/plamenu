"""GoToSocial-side helpers: the disposable peer at gotosocial.local.

The peer is provided by the external live-test harness and federates over its
local Caddy CA; see e2e/README.md for the public harness status.

GoToSocial speaks the Mastodon client API, so tests reuse `Api`; only token
acquisition differs. GtS has **no OAuth password grant** — the token comes
from the full authorization-code flow (app -> /oauth/authorize -> HTML login
form -> consent POST -> OOB code -> token). Two more dialect quirks the
helpers below absorb:

- new accounts default to **manual follow approval** (`locked: true`), so a
  remote follow lands as a follow request; `accept_follows()` clears them.
- all AP endpoints enforce authorized fetch, so every Plamenu-side resolve of
  GtS content also exercises Plamenu's signed GETs.

The harness creates the standing admin account `dave`; its token is minted on
demand by this helper.
"""

import re

import requests

from . import config
from .api import Api, ApiError

OOB = "urn:ietf:wg:oauth:2.0:oob"

# Standing GoToSocial account, created by `./dev up gotosocial` (mirrors
# Pleroma's `bob`). Must match GTS_DEV_USER/_PASSWORD in the top-level dev
# script; the sign-in form authenticates by email.
DAVE_NICK = "dave"
DAVE_EMAIL = "dave@gotosocial.local"
DAVE_PASS = "gots-dave-pass-123"

TOKEN_FILE = config.GOTOSOCIAL_DIR / ".dave-token"

# `admin` lets the tests read /api/v1/admin/reports to observe forwarded
# Flag activities (dave is the instance admin).
SCOPES = "read write follow admin"


def reachable() -> bool:
    """Whether the GtS test instance answers (tests skip when it doesn't)."""
    try:
        Api(config.GOTOSOCIAL_URL).get("/api/v1/instance")
        return True
    except (ApiError, requests.RequestException):
        return False


def mint_token(email: str, password: str, scopes: str = SCOPES) -> str:
    """OAuth authorization-code flow against GtS's HTML auth pages.

    GET /oauth/authorize stashes the request in the session and bounces to
    the sign-in form; POST /auth/sign_in (field `username` = the email)
    signs in; the consent form is a bare session-backed POST back to
    /oauth/authorize, which for an OOB redirect URI lands on /oauth/oob
    with the code in the query string.
    """
    api = Api(config.GOTOSOCIAL_URL)
    app = api.post(
        "/api/v1/apps", client_name="plamenu-e2e", redirect_uris=OOB, scopes=scopes
    )
    r = api.http.get(
        f"{api.base_url}/oauth/authorize",
        params={
            "response_type": "code",
            "client_id": app["client_id"],
            "redirect_uri": OOB,
            "scope": scopes,
        },
        timeout=30,
    )
    if not r.ok:
        raise ApiError(f"/oauth/authorize -> {r.status_code}: {r.text[:300]}")
    r = api.http.post(
        f"{api.base_url}/auth/sign_in",
        data={"username": email, "password": password},
        timeout=30,
    )
    if not r.ok:
        raise ApiError(f"/auth/sign_in -> {r.status_code}: {r.text[:300]}")
    r = api.http.post(f"{api.base_url}/oauth/authorize", timeout=30)
    code = re.search(r"[?&]code=([^&]+)", r.url) or re.search(
        r"<samp[^>]*>([^<]+)</samp>|<code[^>]*>([^<]+)</code>", r.text
    )
    if not (r.ok and code):
        raise ApiError(f"consent POST gave no code ({r.status_code}) for {email}")
    token = api.post(
        "/oauth/token",
        grant_type="authorization_code",
        code=next(g for g in code.groups() if g),
        client_id=app["client_id"],
        client_secret=app["client_secret"],
        redirect_uri=OOB,
    )["access_token"]
    return token


def dave() -> Api:
    """Authenticated Mastodon-API client for the standing `dave` account.

    The token is cached across runs (the code flow needs four round trips);
    a stale or wrong-scope cache re-mints transparently — the cache file
    holds the scope string on the first line and the token on the second.
    """
    if TOKEN_FILE.is_file():
        cached = TOKEN_FILE.read_text().splitlines()
        if len(cached) == 2 and cached[0] == SCOPES:
            api = Api(config.GOTOSOCIAL_URL, token=cached[1].strip())
            try:
                api.get("/api/v1/accounts/verify_credentials")
                return api
            except ApiError:
                pass
    token = mint_token(DAVE_EMAIL, DAVE_PASS)
    TOKEN_FILE.write_text(f"{SCOPES}\n{token}\n")
    return Api(config.GOTOSOCIAL_URL, token=token)


def known_status(api: Api, uri: str) -> dict | None:
    """The status as GtS already knows it (search with resolve=false), or
    None. Unlike `resolve_status` this never makes GtS fetch the URI, so it
    observes inbox ingest rather than triggering a dereference — and unlike
    the home timeline it bypasses GtS's timeline caches, which a failed
    remote delete wedges until restart (GtS 0.22 edit-then-delete bug, see
    tests/test_gotosocial_federation.py)."""
    found = api.get("/api/v2/search", q=uri, resolve="false", type="statuses")[
        "statuses"
    ]
    return found[0] if found else None


def accept_follows(api: Api) -> list[dict]:
    """Authorize every pending follow request; the accepted accounts.

    GtS accounts default to manual approval, so a remote follow parks in
    /api/v1/follow_requests until accepted.
    """
    accepted = []
    for account in api.get("/api/v1/follow_requests"):
        api.post(f"/api/v1/follow_requests/{account['id']}/authorize")
        accepted.append(account)
    return accepted


def set_locked(api: Api, locked: bool) -> dict:
    """Toggle manual follow approval for the authenticated account."""
    return api.patch("/api/v1/accounts/update_credentials", locked=str(locked).lower())
