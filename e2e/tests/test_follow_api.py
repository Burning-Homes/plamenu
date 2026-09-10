"""Follow lifecycle driven through Plamenu's Mastodon client API.

The client-API counterpart of test_follow.py: a Plamenu user looks up,
follows and unfollows alice@mastodon.local via the HTTP API, and the
Mastodon side must observe the outbound Follow / Undo(Follow).
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_follows_and_unfollows_plamenu_user"
)
def test_follow_lifecycle_through_client_api(plamenu_user, plamenu_api, alice, cli):
    """Covers: GET /accounts/lookup, GET /accounts/{id}/statuses (page + pinned),
    POST /accounts/{id}/follow and /unfollow, GET /accounts/relationships,
    GET /accounts/{id}/following + the AP following collection,
    outbound Follow -> inbound Accept, outbound Undo(Follow)."""
    with step(
        f"ensure {config.ALICE} is known to Plamenu (lookup, mention-seeding if not)"
    ):
        account = plamenu_api.lookup(config.ALICE)
        if account is None:
            # lookup never webfingers (Mastodon semantics); a mention resolves her
            cli.post(plamenu_user.username, f"wiring up @{config.ALICE} for an e2e run")
            account = wait_for(
                lambda: plamenu_api.lookup(config.ALICE),
                desc="the mention to seed alice into plamenu's accounts",
            )
        alice_id = account["id"]
        log(f"alice is plamenu account id {alice_id}")

    with step("account statuses endpoint (regular page + pinned)"):
        page = plamenu_api.account_statuses(alice_id, limit=5)
        assert isinstance(page, list), f"statuses page should be a list, got {page!r}"
        pinned = plamenu_api.account_statuses(alice_id, pinned="true")
        assert pinned == [], f"pinned statuses should be an empty list, got {pinned!r}"

    with step("follow alice through the client API (Follow out, Accept back in)"):
        plamenu_api.follow(alice_id)
        wait_for(
            lambda: plamenu_api.relationship(alice_id)["following"],
            desc="plamenu relationship to become following=true (Accept processed)",
        )

    with step("alice appears in the following listings (client API + AP collection)"):
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        following = plamenu_api.following(me["id"])
        assert [f["acct"] for f in following] == [config.ALICE], (
            f"client-API following list should be exactly [alice], got {following!r}"
        )
        page = plamenu_api.ap_get(f"/users/{plamenu_user.username}/following?page=1")
        assert page["orderedItems"] == [following[0]["uri"]], (
            f"AP following page should list alice's actor, got {page['orderedItems']!r}"
        )

    with step("Mastodon sees the follow"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve our user"
        wait_for(
            lambda: alice.relationship(remote["id"])["followed_by"],
            desc=f"alice to see followed_by=true from @{plamenu_user.acct}",
        )

    with step("unfollow through the client API (Undo(Follow) federates)"):
        plamenu_api.unfollow(alice_id)
        wait_for(
            lambda: alice.relationship(remote["id"])["followed_by"] is False,
            desc="alice to stop seeing the follow after Undo",
        )
        assert plamenu_api.relationship(alice_id)["following"] is False, (
            "plamenu relationship still following after unfollow"
        )
