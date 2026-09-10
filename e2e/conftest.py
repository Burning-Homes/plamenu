"""Shared fixtures for the e2e suite.

Session-scoped: stack preflight, alice's Mastodon client, the Plamenu DB.
Function-scoped: a fresh Plamenu user (and client-API login) per test, so
tests are independent and re-runnable without cleanup.
"""

import time

pytest_plugins = ["plamenu_e2e.release_policy"]

import psycopg
import pytest
import requests
from plamenu_e2e import (
    config,
    discourse,
    ephemeral,
    funkwhale,
    gotosocial,
    hubzilla,
    lemmy,
    mastodon,
    mitra,
    mobilizon,
    onion,
    plamenu,
    pleroma,
    plup,
    sharkey,
    unique,
)
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.db import Db


@pytest.fixture(scope="session")
def db() -> Db:
    dbh = Db(config.DB_DSN)
    yield dbh
    dbh.close()


@pytest.fixture(scope="session", autouse=True)
def _preflight(db):
    """Fail fast, with instructions, when the dev stack isn't up."""
    problems = []
    for name, url in (
        ("plamenu", config.PLAMENU_URL),
        ("mastodon", config.MASTODON_URL),
    ):
        try:
            Api(url).get("/api/v1/instance")
        except (ApiError, requests.RequestException) as exc:
            problems.append(f"{name} is not answering at {url}: {exc}")
    try:
        db.conn.execute("SELECT 1")
    except psycopg.Error as exc:
        problems.append(f"plamenu db is not reachable at {config.DB_DSN}: {exc}")
    if problems:
        pytest.exit(
            "e2e prerequisites missing:\n  - "
            + "\n  - ".join(problems)
            + "\nConfigure the public peer harness described in e2e/README.md.",
            returncode=2,
        )


@pytest.fixture(scope="session", autouse=True)
def _suite_instance_settings(db, _preflight):
    """Pin the instance settings the suite depends on for the run.

    * Rate limiting off: every test drives the API from a single
      client IP, so the whole suite shares one rate-limit bucket —
      app-registration (5/10 min/IP) and login limits would otherwise 429
      most tests at fixture setup. Rate limiting has its own coverage in
      `crates/server/tests/rate_limit.rs`.
    * Anonymous previews + public search on: the dev stack's historic open
      posture (settings-owned since migration 0013, private by default).

    The gates read these through a 5 s settings cache, so we wait out that
    TTL before the first test runs, and restore the values after.
    """
    previous = db.conn.execute(
        "SELECT rate_limiting_enabled, timeline_preview_federated,"
        "       timeline_preview_local, timeline_preview_tag, public_search"
        " FROM instance_settings"
    ).fetchone()
    db.conn.execute(
        "UPDATE instance_settings SET rate_limiting_enabled = false,"
        " timeline_preview_federated = true, timeline_preview_local = true,"
        " timeline_preview_tag = true, public_search = true"
    )
    # Outlast the SettingsCache TTL so the changes are live before any request.
    time.sleep(6)
    yield
    if previous is not None:
        db.conn.execute(
            "UPDATE instance_settings SET rate_limiting_enabled = %s,"
            " timeline_preview_federated = %s, timeline_preview_local = %s,"
            " timeline_preview_tag = %s, public_search = %s",
            tuple(previous),
        )


@pytest.fixture(scope="session")
def alice() -> Api:
    """Mastodon client authenticated as alice@mastodon.local (token minted
    once per session — the rails-runner mint takes ~10s)."""
    return mastodon.api_as(config.ALICE_EMAIL)


@pytest.fixture(scope="session")
def cli() -> plamenu.Cli:
    return plamenu.Cli()


@pytest.fixture(scope="session")
def pleroma_bob() -> Api:
    """Mastodon-API client for the standing Pleroma user `bob`. Skips the test
    when the optional Pleroma instance (../pleroma-test) isn't running."""
    if not pleroma.reachable():
        pytest.skip(
            "pleroma test instance not reachable at "
            f"{config.PLEROMA_URL} — start it with the pleroma-test stack"
        )
    return pleroma.bob()


@pytest.fixture(scope="session")
def plup_mari() -> Api:
    """Mastodon-API client for the standing upstream-Pleroma user `mari`. Skips
    the test when the optional upstream-Pleroma instance (../pleroma-upstream-test)
    isn't running. This peer is the RFC 9421 black-hole regression guard —
    real upstream Pleroma, distinct from the Akkoma `pleroma_bob` peer."""
    if not plup.reachable():
        pytest.skip(
            "upstream-pleroma (plup) test instance not reachable at "
            f"{config.PLUP_URL} — start it with `./dev up plup`"
        )
    return plup.mari()


@pytest.fixture(scope="session")
def sharkey_carol() -> sharkey.Sharkey:
    """Misskey-API client for the standing Sharkey user `carol`. Skips the test
    when the optional Sharkey instance (../sharkey-test) isn't running."""
    if not sharkey.reachable():
        pytest.skip(
            "sharkey test instance not reachable at "
            f"{config.SHARKEY_URL} — start it with the sharkey-test stack"
        )
    return sharkey.carol()


@pytest.fixture(scope="session")
def gts_dave() -> Api:
    """Mastodon-API client for the standing GoToSocial user `dave`. Skips the
    test when the optional GtS instance (../gotosocial-test) isn't running."""
    if not gotosocial.reachable():
        pytest.skip(
            "gotosocial test instance not reachable at "
            f"{config.GOTOSOCIAL_URL} — see e2e/README.md"
        )
    return gotosocial.dave()


@pytest.fixture(scope="session")
def mitra_erin() -> Api:
    """Mastodon-API client for standing Mitra user `erin`; optional peer."""
    if not mitra.reachable():
        pytest.skip(
            "mitra test instance not reachable at "
            f"{config.MITRA_URL} — see e2e/README.md"
        )
    return mitra.erin()


@pytest.fixture(scope="session")
def onion_nina() -> Api:
    """Mastodon-API client for `nina` on the onion-identity Mitra; optional
    peer (`./dev up onion`). Plamenu can reach this peer's federation surface
    only through the Tor SOCKS lane — that transport is what its tests cover."""
    if not onion.reachable():
        pytest.skip(
            "onion-mitra test instance not reachable at "
            f"{config.ONION_MITRA_URL} — run ./dev up onion"
        )
    return onion.nina()


@pytest.fixture(scope="session")
def lemmy_frank() -> lemmy.LemmyApi:
    """Lemmy-API client for standing Lemmy admin `frank`; optional peer."""
    if not lemmy.reachable():
        pytest.skip(
            "lemmy test instance not reachable at "
            f"{config.LEMMY_URL} — see e2e/README.md"
        )
    return lemmy.frank()


@pytest.fixture(scope="session")
def mobilizon_grace() -> mobilizon.MobilizonApi:
    """GraphQL client for standing Mobilizon admin `grace`; optional peer.

    Mobilizon is the reference *event* host — the only peer in the fleet that
    speaks the participation verb family (`Join`/`Accept`/`Reject`/`Leave`)."""
    if not mobilizon.reachable():
        pytest.skip(
            "mobilizon test instance not reachable at "
            f"{config.MOBILIZON_URL} — start it with `./dev up mobilizon`"
        )
    return mobilizon.grace()


@pytest.fixture(scope="session")
def mobilizon_group(mobilizon_grace) -> dict:
    """The standing `!plamenu_events` group on the peer.

    Every event that must reach Plamenu through a follow is attributed to a
    group: Mobilizon refuses to be followed as a Person (`:person_no_follow`),
    so a group is the only non-relay channel."""
    return mobilizon_grace.group()


@pytest.fixture(scope="session")
def discourse_diana() -> discourse.DiscourseApi:
    """Browser-session client for the standing Discourse administrator.

    The source-backed peer runs the official ActivityPub plugin unchanged;
    this fixture uses only Discourse's public browser API to create and inspect
    realistic category topics.
    """
    if not discourse.reachable():
        pytest.skip(
            "discourse test instance not reachable at "
            f"{config.DISCOURSE_URL} — start it with `./dev up discourse`"
        )
    return discourse.diana()


@pytest.fixture(scope="session")
def hubzilla_hazel() -> dict:
    """Standing Hubzilla actor; optional native file-object peer."""
    if not hubzilla.reachable():
        pytest.skip(
            "hubzilla test instance not reachable at "
            f"{config.HUBZILLA_URL} — start it with `./dev up hubzilla`"
        )
    return hubzilla.actor()


@pytest.fixture(scope="session")
def funkwhale_fiona() -> funkwhale.FunkwhaleApi:
    """Public-API client for the standing Funkwhale administrator."""
    if not funkwhale.reachable():
        pytest.skip(
            "funkwhale test instance not reachable at "
            f"{config.FUNKWHALE_URL} — start it with `./dev up funkwhale`"
        )
    return funkwhale.fiona()


@pytest.fixture(scope="session")
def plamenu2(db) -> ephemeral.Instance:
    """A second Plamenu instance (https://plamenu2.local), started for the
    session from this very tree and torn down after it.

    Every other peer is foreign software that implements part of what this
    server emits; this one implements all of it, so the Plamenu-to-Plamenu
    tests are the only ones that can hold both halves of a dialect to
    account at once.

    The peer is a fresh install each session (new database, new keys), so the
    dev instance is first made to forget the previous one — otherwise its
    abandoned fetch budget and cached keys would greet the new server."""
    db.forget_host(ephemeral.PEER_DOMAIN)
    instance = ephemeral.peer()
    instance.start()
    instance.apply_suite_settings()
    yield instance
    instance.stop()


@pytest.fixture
def plamenu2_user(plamenu2) -> plamenu.User:
    """A fresh account on the second Plamenu instance, unique per test."""
    return plamenu2.account()


@pytest.fixture
def plamenu2_api(plamenu2, plamenu2_user) -> Api:
    """Client-API session for `plamenu2_user` on the second instance."""
    return plamenu2.login(plamenu2_user)


@pytest.fixture
def plamenu_user(cli) -> plamenu.User:
    """A fresh local Plamenu account (with login credentials), unique per test."""
    user = plamenu.User()
    cli.account_add(user)
    return user


@pytest.fixture
def plamenu_api(plamenu_user) -> Api:
    """Plamenu client-API session for `plamenu_user` (full OAuth flow)."""
    return plamenu.login(plamenu_user)


@pytest.fixture
def plamenu_admin() -> tuple[plamenu.User, Api]:
    """A fresh local `Owner` account and an admin-scoped client-API session
    (`admin:read`/`admin:write`), for the `/api/v1/admin/*` surface."""
    return plamenu.admin()


@pytest.fixture
def marker() -> str:
    """Unique token to embed in posts and find again on the other side."""
    return unique("e2emarker")
