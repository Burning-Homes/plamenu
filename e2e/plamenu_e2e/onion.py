"""Helpers for the onion-identity Mitra peer behind the tor-test rig.

A second Mitra instance whose ``instance_url`` is ``http://<generated>.onion``
(`./dev up onion`). The test harness drives its Mastodon-compatible client
API directly at ``http://127.0.0.1:8381`` — no Tor needed from Python — while
Plamenu can only reach it through the Tor SOCKS lane, which is exactly the
transport under test.
"""

import requests

from . import config
from .api import Api, ApiError
from .mitra import MitraApi

OOB = "urn:ietf:wg:oauth:2.0:oob"
NINA_NICK = "nina"
NINA_PASS = "mitra-nina-pass-123"
TOKEN_FILE = config.TOR_DIR / ".nina-token"
SCOPES = "read write follow"


def reachable() -> bool:
    try:
        MitraApi(config.ONION_MITRA_URL).get("/api/v1/instance")
        return True
    except (ApiError, requests.RequestException):
        return False


def onion_domain() -> str:
    """The generated .onion hostname — the peer's federation identity."""
    uri = MitraApi(config.ONION_MITRA_URL).get("/api/v1/instance")["uri"]
    return uri.removeprefix("http://").removeprefix("https://").strip("/")


def mint_token() -> str:
    api = MitraApi(config.ONION_MITRA_URL)
    app = api.post(
        "/api/v1/apps",
        client_name="plamenu-e2e",
        redirect_uris=OOB,
        scopes=SCOPES,
    )
    return api.post(
        "/oauth/token",
        grant_type="password",
        username=NINA_NICK,
        password=NINA_PASS,
        client_id=app["client_id"],
        client_secret=app["client_secret"],
        scope=SCOPES,
    )["access_token"]


def nina() -> Api:
    if TOKEN_FILE.is_file():
        api = MitraApi(config.ONION_MITRA_URL, token=TOKEN_FILE.read_text().strip())
        try:
            api.get("/api/v1/accounts/verify_credentials")
            return api
        except ApiError:
            pass
    token = mint_token()
    TOKEN_FILE.write_text(f"{token}\n")
    return MitraApi(config.ONION_MITRA_URL, token=token)
