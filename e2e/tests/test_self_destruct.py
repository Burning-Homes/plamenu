"""Server self-destruct (Mastodon's `tootctl self-destruct`), end to end.

Runs against an *ephemeral* second Plamenu instance (https://doomed.local,
`plamenu_e2e/ephemeral.py`) because a self-destruct is instance-wide and
irreversible — the standing https://plamenu.local instance must survive the
suite. The real Mastodon peer observes the wind-down: every local account's
`Delete(Actor)` must reach every *known* inbox (not just followers'), and
the doomed server itself must answer 410 Gone except for sign-in and the
data export.
"""

import pytest
import requests
from plamenu_e2e import ephemeral, unique
from plamenu_e2e.steps import log, step, wait_for


@pytest.fixture
def doomed():
    instance = ephemeral.Instance()
    instance.start()
    yield instance
    instance.stop()


def http_get(path: str, **kwargs) -> requests.Response:
    return requests.get(ephemeral.URL + path, verify=False, timeout=30, **kwargs)


def deleted_on_mastodon(api, account_id: str) -> bool:
    """Mastodon marks a remotely-deleted account `suspended` (tombstone kept)."""
    account = api.account(account_id)
    return account is None or bool(account.get("suspended"))


def follow_severed(api, account_id: str) -> bool:
    """Mastodon drops a deleted account from the relationships endpoint
    entirely (empty array) — either that or `following: false` proves it."""
    relationships = api.get("/api/v1/accounts/relationships", **{"id[]": account_id})
    return not relationships or not relationships[0]["following"]


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_account_delete_reaches_plamenu"
)
def test_self_destruct_broadcasts_deletions_and_gates_the_server(alice, doomed, marker):
    """Covers: the `plamenu self-destruct` CLI (arming + progress report),
    the Delete(Actor) broadcast to all known inboxes — a followed and a
    merely-known account both end up suspended on Mastodon — the severed
    follow, the 410 gate over API/AP/web, and its sign-in/export allowlist."""
    victim1, victim2 = unique("victim"), unique("victim")

    with step("seed the doomed instance: two accounts, one federated post"):
        doomed.cli("account", "add", victim1)
        doomed.cli("account", "add", victim2)
        doomed.cli("post", victim1, f"So long, and thanks for all the fish {marker}")

    with step(f"alice follows @{victim1}@doomed.local (Mastodon learns the inbox)"):
        followed = alice.resolve_account(f"{victim1}@{ephemeral.DOMAIN}")
        assert followed, "Mastodon could not resolve the doomed account"
        alice.follow(followed["id"])
        wait_for(
            lambda: alice.relationship(followed["id"])["following"],
            desc="alice's follow of the doomed account to be accepted",
        )

    with step(f"alice merely resolves @{victim2}@doomed.local, never follows it"):
        known = alice.resolve_account(f"{victim2}@{ephemeral.DOMAIN}")
        assert known, "Mastodon could not resolve the second doomed account"

    with step("arm self-destruct through the CLI (typed domain + explicit yes)"):
        # A wrong domain must refuse to arm.
        with pytest.raises(RuntimeError, match="Domains do not match"):
            doomed.cli("self-destruct", input_text="wrong.example\n")
        out = doomed.cli("self-destruct", input_text=f"{ephemeral.DOMAIN}\nyes\n")
        assert "Self-destruct enabled" in out, out

    with step("Mastodon suspends the followed account and severs the follow"):
        wait_for(
            lambda: deleted_on_mastodon(alice, followed["id"]),
            desc="Mastodon to suspend the followed doomed account (Delete processed)",
        )
        wait_for(
            lambda: follow_severed(alice, followed["id"]),
            desc="the follow of the deleted account to be severed on Mastodon",
        )

    with step("…and the never-followed account too (audience = every known inbox)"):
        wait_for(
            lambda: deleted_on_mastodon(alice, known["id"]),
            desc="Mastodon to suspend the never-followed doomed account",
        )

    with step("both local accounts are tombstoned (suspended) in the doomed db"):
        assert (
            doomed.db_value(
                "SELECT count(*) FROM accounts WHERE domain IS NULL AND suspended_at IS NULL"
            )
            == 0
        )

    with step("the doomed server answers 410 Gone across API, AP and web"):
        r = http_get("/api/v1/instance")
        assert (r.status_code, r.json()) == (410, {"error": "Gone"})
        r = http_get(
            f"/users/{victim1}", headers={"Accept": "application/activity+json"}
        )
        assert r.status_code == 410, "the actor must read as gone to remotes"
        r = http_get("/")
        assert r.status_code == 410
        assert "permanently going offline" in r.text

    with step("…except the wind-down allowlist: sign-in, export, health"):
        for path in ("/login", "/settings/export", "/health", "/ready"):
            r = http_get(path)
            assert r.status_code != 410, (
                f"{path} must stay reachable, got {r.status_code}"
            )
            log(f"{path} -> {r.status_code}")

    with step("re-running the CLI reports the wind-down through to completion"):
        out = doomed.cli("self-destruct")
        assert "already enabled" in out, out
        out = wait_for(
            lambda: (
                (o := doomed.cli("self-destruct"))
                and "Every deletion notice has been sent" in o
                and o
            ),
            desc="the delivery queue to drain and the CLI to report completion",
        )
        log(out.strip().splitlines()[-1])
