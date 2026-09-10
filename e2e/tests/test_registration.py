"""Self-service account lifecycle (M21), observed end-to-end: API sign-up
with the confirmation mail read from mailpit, invite-based sign-up while
registrations are closed, and the password-reset web flow.

Requires the mailpit container from `compose.dev.yml` (SMTP :1025, HTTP API
:8025) and Plamenu started with the SMTP test configuration.
"""

import dataclasses
import re
import time

import pytest
import requests
from plamenu_e2e import config, plamenu, unique
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.steps import wait_for

MAILPIT_URL = "http://127.0.0.1:8025"
OOB = "urn:ietf:wg:oauth:2.0:oob"


@pytest.fixture(scope="session", autouse=True)
def _mailpit_reachable():
    try:
        requests.get(f"{MAILPIT_URL}/api/v1/messages", timeout=5)
    except Exception as exc:  # noqa: BLE001 - any connection trouble means skip
        pytest.skip(f"mailpit is not answering at {MAILPIT_URL}: {exc}")


@pytest.fixture
def registrations_mode(db):
    """Set `instance_settings.registrations_mode` for the test (there is no
    REST setting endpoint — the admin web form or SQL are the only ways),
    restoring the previous mode afterwards."""
    previous = db.conn.execute(
        "SELECT registrations_mode FROM instance_settings"
    ).fetchone()[0]

    def set_mode(mode: str) -> None:
        db.conn.execute("UPDATE instance_settings SET registrations_mode = %s", (mode,))

    yield set_mode
    set_mode(previous)


def mailpit_link(recipient: str, subject_part: str, param: str) -> str:
    """The first `https://…?{param}=…` link in the newest mailpit message to
    `recipient` whose subject contains `subject_part`."""

    def find():
        listing = requests.get(f"{MAILPIT_URL}/api/v1/messages", timeout=10).json()
        for message in listing.get("messages", []):
            if subject_part not in message["Subject"]:
                continue
            if not any(to["Address"] == recipient for to in message["To"]):
                continue
            text = requests.get(
                f"{MAILPIT_URL}/api/v1/message/{message['ID']}", timeout=10
            ).json()["Text"]
            match = re.search(rf"https://\S+\?{param}=[\w~-]+", text)
            if match:
                return match.group(0)
        return None

    return wait_for(find, desc=f"mail to {recipient} ({subject_part!r})")


def app_token() -> str:
    """A client-credentials (app-level) token — what `POST /api/v1/accounts`
    authenticates with."""
    api = Api(config.PLAMENU_URL)
    app = api.post(
        "/api/v1/apps",
        client_name="plamenu-e2e-registration",
        redirect_uris=OOB,
        scopes="read write",
    )
    return api.post(
        "/oauth/token",
        grant_type="client_credentials",
        client_id=app["client_id"],
        client_secret=app["client_secret"],
    )["access_token"]


def sign_up(username: str, email: str, password: str, **extra) -> Api:
    """`POST /api/v1/accounts`; returns a client bound to the new user's
    sign-up token."""
    signup = Api(config.PLAMENU_URL, token=app_token())
    token = signup.post(
        "/api/v1/accounts",
        username=username,
        email=email,
        password=password,
        agreement="true",
        **extra,
    )["access_token"]
    return Api(config.PLAMENU_URL, token=token)


def browse(url: str, **kwargs) -> requests.Response:
    return requests.get(url, verify=False, timeout=30, **kwargs)


def test_signup_confirmation_mail_unlocks_login(registrations_mode):
    registrations_mode("open")
    user = plamenu.User(username=unique("e2ereg"))
    username, email = user.username, user.email
    user_api = sign_up(username, email, user.password)

    # The sign-up token drives the e-mail endpoints but nothing else yet.
    assert user_api.get("/api/v1/emails/check_confirmation") is False
    with pytest.raises(ApiError, match="403"):
        user_api.get("/api/v1/accounts/verify_credentials")

    # The confirmation mail arrives at mailpit; its link confirms.
    link = mailpit_link(email, "Confirmation instructions", "confirmation_token")
    page = browse(link)
    assert page.ok and "E-mail confirmed" in page.text

    # The login is functional now: the token resolves and a fresh OAuth
    # password login works.
    assert user_api.get("/api/v1/accounts/verify_credentials")["username"] == username
    assert user_api.get("/api/v1/emails/check_confirmation") is True
    relogin = plamenu.login(user)
    assert relogin.get("/api/v1/accounts/verify_credentials")["username"] == username

    # The welcome mail follows the confirmation.
    wait_for(
        lambda: any(
            "Welcome" in m["Subject"] and any(t["Address"] == email for t in m["To"])
            for m in requests.get(f"{MAILPIT_URL}/api/v1/messages", timeout=10)
            .json()
            .get("messages", [])
        ),
        desc=f"welcome mail to {email}",
    )


def test_invite_opens_closed_registrations(db, registrations_mode, plamenu_user):
    registrations_mode("none")

    # A closed door for plain sign-ups.
    with pytest.raises(ApiError, match="403"):
        sign_up(unique("e2ecold"), f"{unique('e2ecold')}@example.com", "e2e-password!")

    # An invite from a standing local user (inserted directly — minting via
    # the web form is covered by the integration tests).
    creator_id = db.conn.execute(
        "SELECT u.id FROM users u JOIN accounts a ON a.id = u.account_id"
        " WHERE a.username = %s AND a.domain IS NULL",
        (plamenu_user.username,),
    ).fetchone()[0]
    code = unique("Inv")
    db.conn.execute(
        "INSERT INTO invites (id, user_id, code) VALUES (%s, %s, %s)",
        (time.time_ns(), creator_id, code),
    )

    # The shareable link: JSON bootstrap for apps, sign-up form for browsers.
    bootstrap = browse(
        f"{config.PLAMENU_URL}/invite/{code}",
        headers={"Accept": "application/json"},
    )
    assert bootstrap.ok, bootstrap.text
    assert bootstrap.json()["invite_code"] == code
    form = browse(f"{config.PLAMENU_URL}/invite/{code}")
    assert "Create an account" in form.text

    # The invited sign-up goes through, pre-approved; confirmation still by
    # mail. The invite's use is counted.
    username = unique("e2einv")
    email = f"{username}@example.com"
    user_api = sign_up(username, email, "e2e-password!", invite_code=code)
    link = mailpit_link(email, "Confirmation instructions", "confirmation_token")
    assert "E-mail confirmed" in browse(link).text
    assert user_api.get("/api/v1/accounts/verify_credentials")["username"] == username
    uses = db.conn.execute(
        "SELECT uses FROM invites WHERE code = %s", (code,)
    ).fetchone()[0]
    assert uses == 1


def test_password_reset_flow(plamenu_user, plamenu_api):
    session = requests.Session()
    session.verify = False

    # Request the mail from the login-page flow.
    requested = session.post(
        f"{config.PLAMENU_URL}/auth/password",
        data={"email": plamenu_user.email},
        timeout=30,
    )
    assert "Check your inbox" in requested.text

    link = mailpit_link(
        plamenu_user.email, "Reset password instructions", "reset_password_token"
    )
    form = session.get(link, timeout=30)
    assert "Choose a new password" in form.text

    token = link.split("reset_password_token=", 1)[1]
    done = session.post(
        f"{config.PLAMENU_URL}/auth/password/edit",
        data={
            "reset_password_token": token,
            "password": "a brand new password",
            "password_confirmation": "a brand new password",
        },
        timeout=30,
    )
    assert "Password changed" in done.text

    # Every pre-existing token is revoked, the old password is dead, the new
    # one signs in.
    with pytest.raises(ApiError, match="401"):
        plamenu_api.get("/api/v1/accounts/verify_credentials")
    with pytest.raises(ApiError):
        plamenu.login(plamenu_user)  # old password
    fresh = plamenu.login(
        dataclasses.replace(plamenu_user, password="a brand new password")
    )
    handle = fresh.get("/api/v1/accounts/verify_credentials")["username"]
    assert handle == plamenu_user.username
