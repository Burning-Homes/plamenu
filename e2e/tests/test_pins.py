"""Status pins federate in both directions: a pin is an Add targeting the
account's featured collection, an unpin a Remove, and discovery also works
via the dereferenceable featured collection itself."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pin_federates_from_mastodon"
)
def test_pin_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: POST /statuses/{id}/pin and /unpin, the pinned=true account
    statuses listing, the filled featured AP collection, and the Add/Remove
    fan-out — Mastodon shows (then drops) the pin on the remote profile."""
    with step(f"alice follows @{plamenu_user.acct} (so the Add reaches her)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    with step("post through Plamenu, and the status reaches alice"):
        posted = plamenu_api.post_status(f"Pin-worthy thoughts {marker}")
        assert posted["pinned"] is False
        wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the post to arrive on alice's home timeline",
        )

    with step("pin it; Plamenu lists it as pinned and serves it as featured"):
        pinned = plamenu_api.post(f"/api/v1/statuses/{posted['id']}/pin")
        assert pinned["pinned"] is True

        own = plamenu_api.get("/api/v1/accounts/verify_credentials")
        listing = plamenu_api.account_statuses(own["id"], pinned="true")
        assert [s["id"] for s in listing] == [posted["id"]]

        featured = plamenu_api.ap_get(
            f"/users/{plamenu_user.username}/collections/featured"
        )
        assert featured["totalItems"] == 1
        assert featured["orderedItems"][0]["id"] == posted["uri"]

    with step("Mastodon records the pin on the remote profile (Add)"):
        wait_for(
            lambda: any(
                marker in s["content"]
                for s in alice.account_statuses(account["id"], pinned="true")
            ),
            desc="the pin to appear on the profile as seen from Mastodon",
        )

    with step("unpin; Mastodon drops it again (Remove)"):
        unpinned = plamenu_api.post(f"/api/v1/statuses/{posted['id']}/unpin")
        assert unpinned["pinned"] is False
        wait_for(
            lambda: not alice.account_statuses(account["id"], pinned="true"),
            desc="the pin to disappear from the Mastodon-side profile",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_pin_federates_to_mastodon"
)
def test_pin_federates_from_mastodon(alice, plamenu_user, plamenu_api, cli, db, marker):
    """Covers: inbound Add/Remove targeting the sender's featured collection
    — alice's pin shows up in Plamenu's pinned listing and goes away again."""
    with step(f"@{plamenu_user.username} follows {config.ALICE} (so the Add arrives)"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    with step("alice posts; the status reaches Plamenu"):
        posted = alice.post_status(f"Pinned over the wire {marker}")
        wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )

    with step("alice pins it; Plamenu records the remote pin (Add)"):
        alice.post(f"/api/v1/statuses/{posted['id']}/pin")
        remote_alice = plamenu_api.lookup(config.ALICE)
        assert remote_alice, "Plamenu must already know alice"
        wait_for(
            lambda: any(
                marker in s["content"]
                for s in plamenu_api.account_statuses(remote_alice["id"], pinned="true")
            ),
            desc="the pin to appear in Plamenu's pinned listing",
        )

    with step("alice unpins; Plamenu drops it again (Remove)"):
        alice.post(f"/api/v1/statuses/{posted['id']}/unpin")
        wait_for(
            lambda: (
                not any(
                    marker in s["content"]
                    for s in plamenu_api.account_statuses(
                        remote_alice["id"], pinned="true"
                    )
                )
            ),
            desc="the pin to disappear from Plamenu's pinned listing",
        )
