"""Account migration (`alsoKnownAs` / `Move`) over real federation.

The alias half resolves targets over real webfinger + actor fetch against the
live Mastodon instance, and Plamenu publishes them on its actor document the
way a migration source must — via the CLI and via the `/settings/aliases`
web form. The full outbound move is driven through the
`/settings/migration` web form: a Mastodon follower's server receives the
`Move` and re-follows the new home (Mastodon-side aliases are seeded with a
rails runner, since Mastodon has no API for them).
"""

import re

import pytest
import requests
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.api import Api
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


def test_alias_is_resolved_over_federation_and_published(cli, plamenu_user):
    # Resolving the alias hits Mastodon's webfinger and actor endpoints for real.
    alias_uri = cli.alias_add(plamenu_user.username, config.ALICE)
    assert alias_uri.startswith(config.MASTODON_URL + "/"), alias_uri

    # The actor document a remote server fetches advertises the alias and the
    # JSON-LD term that defines it.
    doc = Api(config.PLAMENU_URL).ap_get(f"/users/{plamenu_user.username}")
    assert alias_uri in doc.get("alsoKnownAs", []), doc.get("alsoKnownAs")
    context = doc["@context"]
    assert any(isinstance(term, dict) and "alsoKnownAs" in term for term in context)


def test_migration_to_unaliased_target_is_refused(cli, plamenu_user):
    # alice has not listed this brand-new Plamenu account as one of her
    # aliases, so the move must be rejected (the anti-hijack guard) and leave
    # no redirect behind.
    try:
        cli.migrate(plamenu_user.username, config.ALICE)
    except RuntimeError as exc:
        assert "alias" in str(exc).lower(), exc
    else:
        raise AssertionError("migration to an unaliased target should fail")

    doc = Api(config.PLAMENU_URL).ap_get(f"/users/{plamenu_user.username}")
    assert "movedTo" not in doc


def test_web_aliases_page_adds_and_removes(plamenu_user):
    """The /settings/aliases form resolves a Mastodon handle over real
    federation, publishes it on the actor document, and removes it again."""
    web = _web_login(plamenu_user)

    with step("add alice as an alias through the web form"):
        page = web.get(f"{config.PLAMENU_URL}/settings/aliases")
        assert page.status_code == 200
        resp = web.post(
            f"{config.PLAMENU_URL}/web/settings/aliases",
            data={"csrf": _csrf(page.text), "acct": config.ALICE},
            allow_redirects=False,
        )
        assert resp.status_code == 303, f"alias add failed: {resp.status_code}"
        assert "saved=1" in resp.headers["location"], resp.headers["location"]

    with step("the alias is listed and published in alsoKnownAs"):
        page = web.get(f"{config.PLAMENU_URL}/settings/aliases")
        assert config.ALICE in page.text
        doc = Api(config.PLAMENU_URL).ap_get(f"/users/{plamenu_user.username}")
        aliases = doc.get("alsoKnownAs", [])
        assert len(aliases) == 1 and aliases[0].startswith(config.MASTODON_URL), aliases
        alias_uri = aliases[0]

    with step("remove it again"):
        resp = web.post(
            f"{config.PLAMENU_URL}/web/settings/aliases/delete",
            data={"csrf": _csrf(page.text), "uri": alias_uri},
            allow_redirects=False,
        )
        assert resp.status_code == 303
        doc = Api(config.PLAMENU_URL).ap_get(f"/users/{plamenu_user.username}")
        assert not doc.get("alsoKnownAs"), doc.get("alsoKnownAs")


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound server-lifecycle: Plamenu's web migration flow moves the account and re-points a Mastodon follower onto the target; no paired reverse.",
)
def test_web_migration_moves_mastodon_follower(plamenu_user, alice):
    """The full outbound move: alice follows a Plamenu account; the account
    moves to a fresh Mastodon account (which lists it as an alias) through
    the /settings/migration form; Mastodon processes the `Move` and alice
    ends up following the new home."""
    target_username = unique("moved")

    with step("create the Mastodon target and let it claim this account"):
        mastodon.create_account(target_username)
        alias_uri = mastodon.add_alias(target_username, plamenu_user.acct)
        assert alias_uri.startswith(config.PLAMENU_URL), alias_uri

    with step("alice follows the Plamenu account"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, f"alice cannot resolve {plamenu_user.acct}"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="alice's follow is accepted",
        )

    with step("move through the /settings/migration web form"):
        web = _web_login(plamenu_user)
        page = web.get(f"{config.PLAMENU_URL}/settings/migration")
        assert "Move to a different account" in page.text
        resp = web.post(
            f"{config.PLAMENU_URL}/web/settings/migration",
            data={
                "csrf": _csrf(page.text),
                "acct": f"{target_username}@{config.MASTODON_DOMAIN}",
                "current_password": plamenu_user.password,
            },
            allow_redirects=False,
        )
        assert resp.status_code == 303, f"move failed: {resp.status_code}"
        assert "saved=1" in resp.headers["location"], resp.headers["location"]

    with step("the redirect is published and the page shows the cooldown"):
        doc = Api(config.PLAMENU_URL).ap_get(f"/users/{plamenu_user.username}")
        assert doc.get("movedTo", "").startswith(config.MASTODON_URL), doc.get(
            "movedTo"
        )
        page = web.get(f"{config.PLAMENU_URL}/settings/migration")
        assert "next move is possible after" in page.text
        assert f"{target_username}@{config.MASTODON_DOMAIN}" in page.text

    with step("Mastodon re-points alice's follow at the new account"):
        target = alice.resolve_account(f"{target_username}@{config.MASTODON_DOMAIN}")
        assert target, "alice cannot resolve the move target"
        wait_for(
            lambda: alice.relationship(target["id"])["following"],
            desc="alice follows the new home after the Move",
        )
        log("alice now follows the migrated-to account")


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound server-lifecycle: a Mastodon Move re-points Plamenu's local follower and records the redirect; no paired reverse.",
)
def test_mastodon_move_reaches_plamenu(plamenu_user, cli, db, marker):
    """Reverse of test_web_migration_moves_mastodon_follower: a Mastodon account
    a Plamenu user follows moves to a fresh destination; Plamenu records the
    redirect and re-points the follow at the destination.

    Uses fresh mover/dest accounts (never alice, whose session must survive)."""
    mover = unique("mover")
    dest = unique("movedest")
    mover_acct = f"{mover}@{config.MASTODON_DOMAIN}"
    dest_acct = f"{dest}@{config.MASTODON_DOMAIN}"

    with step("create a fresh Mastodon mover and move-destination"):
        mastodon.create_account(mover)
        mastodon.create_account(dest)

    with step("dest claims the mover as an alias (anti-hijack requirement)"):
        mastodon.add_alias(dest, mover_acct)

    with step("a Plamenu user follows the mover so it is in mover.followers"):
        cli.follow(plamenu_user.username, mover_acct)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow of the mover to be accepted",
        )

    with step("the mover Moves to dest (rails: AccountMigration + MoveService)"):
        dest_uri = mastodon.trigger_move(mover, dest_acct)
        log(f"moved to {dest_uri}")

    with step("Plamenu records the redirect and re-points the follower"):
        wait_for(
            lambda: db.remote_moved_to(mover, config.MASTODON_DOMAIN) == dest_uri,
            desc="the mover's moved_to_uri to be recorded",
        )
        wait_for(
            lambda: db.outbound_follow_exists(
                plamenu_user.username, dest, config.MASTODON_DOMAIN
            ),
            desc="the follower to be re-pointed at the destination",
        )
        assert not db.outbound_follow_exists(
            plamenu_user.username, mover, config.MASTODON_DOMAIN
        ), "the old follow of the mover must be retired"
