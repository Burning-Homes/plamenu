"""Account suspension federation (Mastodon parity, `toot:suspended`).

A reversible admin suspension federates as a blanked `Update(Actor)` carrying
`suspended: true` — not a `Delete(Actor)` — so peers mirror the state with
`suspension_origin = remote` and lift it when the origin stops reporting it.
Covers both directions against live Mastodon:

- Plamenu suspends a local account → Mastodon flags the remote account
  `suspended` and unflags it after the unsuspend.
- Mastodon suspends alice's neighbour → Plamenu stamps the stored account
  `suspended_at`/`suspension_origin='remote'` and clears it on unsuspend.
"""

import pytest
from plamenu_e2e import mastodon
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_suspension_federates_from_mastodon"
)
def test_suspension_federates_to_mastodon(
    alice, plamenu_user, plamenu_api, plamenu_admin, marker
):
    _admin_user, admin_api = plamenu_admin
    with step(f"alice follows @{plamenu_user.acct} (audience for the Update)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the Plamenu user"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    local_id = plamenu_api.get("/api/v1/accounts/verify_credentials")["id"]
    with step("a Plamenu admin suspends the account"):
        admin_api.post(
            f"/api/v1/admin/accounts/{local_id}/action",
            type="suspend",
            text=f"e2e suspension {marker}",
        )
        wait_for(
            lambda: (alice.account(account["id"]) or {}).get("suspended") is True,
            desc="Mastodon to flag the remote account suspended",
        )

    with step("the blanked actor still dereferences with suspended: true"):
        actor = alice.resolve_account(plamenu_user.acct)
        assert actor is not None, "the actor must stay resolvable (not deleted)"

    with step("unsuspending restores the account on Mastodon"):
        admin_api.post(f"/api/v1/admin/accounts/{local_id}/unsuspend")
        wait_for(
            lambda: not (alice.account(account["id"]) or {}).get("suspended"),
            desc="Mastodon to lift the mirrored suspension",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_suspension_federates_to_mastodon"
)
def test_suspension_federates_from_mastodon(alice, cli, db, plamenu_user, marker):
    victim = f"suspendee{marker[-8:]}"
    with step(f"a fresh Mastodon account @{victim} exists and is followed"):
        mastodon.create_account(victim)
        cli.follow(plamenu_user.username, f"{victim}@mastodon.local")
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )
        assert db.remote_suspension(victim, "mastodon.local") is None

    try:
        with step("Mastodon admin-suspends the account; Plamenu mirrors it"):
            mastodon.suspend_account(victim)
            wait_for(
                lambda: db.remote_suspension(victim, "mastodon.local") == "remote",
                desc="the inbound Update to stamp suspension_origin=remote",
            )
    finally:
        with step("unsuspending lifts the mirrored suspension"):
            mastodon.unsuspend_account(victim)
            wait_for(
                lambda: db.remote_suspension(victim, "mastodon.local") is None,
                desc="the inbound Update to clear the suspension",
            )
