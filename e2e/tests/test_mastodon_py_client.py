"""Client-library compatibility: Mastodon.py (pinned 2.2.1) driving Plamenu.

Two credential paths, both ending in real API traffic through the library:

* the classic programmatic path — ``Mastodon.create_app`` against
  ``POST /api/v1/apps``, then the OAuth authorization-code flow via
  ``log_in(code=...)`` (Plamenu has no password grant, matching modern
  Mastodon's browser-only sign-in);
* the web Development page — an application created on
  ``/settings/applications/new``, whose one-time revealed access token is
  handed straight to ``Mastodon(access_token=...)``.

TLS verification is off (local Caddy CA, same posture as api.py).
"""

import re

import pytest
import requests
from mastodon import Mastodon
from plamenu_e2e import config, plamenu, unique
from plamenu_e2e.steps import step

OOB = "urn:ietf:wg:oauth:2.0:oob"
SCOPES = ["read", "write"]


def insecure_session() -> requests.Session:
    """A requests session that accepts the dev stack's local CA."""
    session = requests.Session()
    session.verify = False
    return session


@pytest.fixture(scope="module")
def user() -> plamenu.User:
    account = plamenu.User()
    plamenu.Cli().account_add(account)
    return account


def consent_code(client_id: str, user: plamenu.User) -> str:
    """Submit the /oauth/authorize consent form the way a browser would and
    return the out-of-band authorization code."""
    page = requests.post(
        f"{config.PLAMENU_URL}/oauth/authorize",
        data={
            "client_id": client_id,
            "redirect_uri": OOB,
            "scope": " ".join(SCOPES),
            "email": user.email,
            "password": user.password,
        },
        verify=False,
        timeout=30,
    )
    code = re.search(r"<pre[^>]*>(.+?)</pre>", page.text)
    assert page.ok and code, f"consent gave no code ({page.status_code})"
    return code.group(1)


def exercise_api(client: Mastodon, user: plamenu.User) -> None:
    """The shared workout: identity, posting, reading back, cleanup."""
    with step("verify_credentials sees the right account"):
        me = client.account_verify_credentials()
        assert me.username == user.username

    with step("instance metadata parses into Mastodon.py entities"):
        instance = client.instance()
        assert config.PLAMENU_DOMAIN in (
            getattr(instance, "domain", "") or getattr(instance, "uri", "")
        )

    with step("post a status and read it back from the home timeline"):
        marker = unique("mpy")
        status = client.status_post(f"Mastodon.py speaks Plamenu {marker}")
        assert marker in status.content
        home = client.timeline_home()
        assert any(marker in entry.content for entry in home)

    with step("favourite, then delete"):
        favourited = client.status_favourite(status)
        assert favourited.favourited
        client.status_delete(status)
        assert not any(entry.id == status.id for entry in client.timeline_home())


def test_create_app_and_oauth_code_login(user):
    with step("register an application via Mastodon.create_app"):
        client_id, client_secret = Mastodon.create_app(
            unique("mpyapp"),
            scopes=SCOPES,
            redirect_uris=OOB,
            website="https://example.com/",
            api_base_url=config.PLAMENU_URL,
            session=insecure_session(),
        )
        assert client_id and client_secret

    with step("sign in over the authorization-code flow"):
        client = Mastodon(
            client_id=client_id,
            client_secret=client_secret,
            api_base_url=config.PLAMENU_URL,
            session=insecure_session(),
        )
        # Also proves auth_request_url() builds against Plamenu's endpoints.
        assert "/oauth/authorize" in client.auth_request_url(
            scopes=SCOPES, redirect_uris=OOB
        )
        token = client.log_in(
            code=consent_code(client_id, user), scopes=SCOPES, redirect_uri=OOB
        )
        assert token

    exercise_api(client, user)


def test_development_page_token_drives_mastodon_py(user):
    browser = insecure_session()

    with step("sign in to the web UI"):
        landed = browser.post(
            f"{config.PLAMENU_URL}/login",
            data={"email": user.email, "password": user.password},
            allow_redirects=False,
            timeout=30,
        )
        assert landed.status_code == 303, landed.status_code

    with step("the applications list links to a separate creation page"):
        index = browser.get(f"{config.PLAMENU_URL}/settings/applications", timeout=30)
        assert index.ok
        assert "/settings/applications/new" in index.text
        assert 'name="name"' not in index.text, "create form must not be inline"

    with step("create an application on the dedicated page"):
        form = browser.get(
            f"{config.PLAMENU_URL}/settings/applications/new", timeout=30
        )
        csrf = re.search(r'name="csrf" value="([^"]+)"', form.text).group(1)
        reveal = browser.post(
            f"{config.PLAMENU_URL}/web/settings/applications",
            data=[
                ("csrf", csrf),
                ("name", unique("mpyweb")),
                ("website", ""),
                ("redirect_uris", OOB),
                ("scopes", "read"),
                ("scopes", "write"),
            ],
            timeout=30,
        )
        assert reveal.ok
        token = re.search(
            r"Your access token</th><td><code>([^<]+)</code>", reveal.text
        )
        assert token, "reveal page must show the access token once"

    with step("hand the revealed token to Mastodon.py"):
        client = Mastodon(
            access_token=token.group(1),
            api_base_url=config.PLAMENU_URL,
            session=insecure_session(),
        )

    exercise_api(client, user)
