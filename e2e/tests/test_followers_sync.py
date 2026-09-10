"""FEP-8fcf followers synchronization against real Mastodon."""

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound followers-sync repair: on private delivery Plamenu emits a Collection-Synchronization header that lets a Mastodon receiver drop a ghost follower; a delivery-side mechanism with no paired reverse.",
)
def test_private_delivery_repairs_mastodon_ghost_follower(
    alice, plamenu_user, plamenu_api, db, marker
):
    """A missed Undo(Follow) leaves Mastodon thinking Alice follows a Plamenu
    account. A later followers-only delivery with Collection-Synchronization
    must make Mastodon fetch Plamenu's signed partial collection and remove
    that stale local follow."""

    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the Plamenu account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="Mastodon relationship to become following=true",
        )
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="Plamenu to record Alice as a follower",
        )

    with step("delete only Plamenu's follower row to simulate a missed Undo(Follow)"):
        db.delete_remote_follower(
            plamenu_user.username, "alice", config.MASTODON_DOMAIN
        )
        assert db.follower_count(plamenu_user.username) == 0
        assert alice.relationship(account["id"])["following"], (
            "Mastodon should still have the stale follow before synchronization"
        )

    with step("send a private mentioned post so Mastodon receives the sync header"):
        plamenu_api.post_status(
            f"@{config.ALICE} followers sync repair {marker}",
            visibility="private",
        )
        wait_for(
            lambda: not alice.relationship(account["id"])["following"],
            desc="Mastodon to remove the stale follow after fetching the partial collection",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound followers-sync repair: Plamenu consumes a Mastodon Collection-Synchronization header and drops its own ghost follower; a receive-side mechanism with no paired reverse.",
)
def test_private_delivery_repairs_plamenu_ghost_follower(
    plamenu_user, plamenu_api, cli, db, marker
):
    """Reverse direction: Plamenu is the one consuming Collection-Synchronization.
    A Plamenu user follows a Mastodon account; that account's follower row is
    destroyed on the Mastodon side (a missed Undo(Follow)) while Plamenu keeps
    the ghost. The account's next followers-only delivery carries a
    Collection-Synchronization header; Plamenu fetches its now-empty scoped
    followers collection and drops the ghost follow.

    A FRESH Mastodon account is used (not the shared `alice`): the sync compares
    the sender's *whole* plamenu.local follower set, and alice accumulates stale
    throwaway followers across the suite, which would keep her partial collection
    non-empty and mask the removal. A fresh account's only follower is this test's
    user, so its collection cleanly empties."""
    victim = unique("ghosthost")
    victim_acct = f"{victim}@{config.MASTODON_DOMAIN}"

    with step("create a fresh Mastodon account and have a Plamenu user follow it"):
        mastodon.create_account(victim)
        victim_api = mastodon.api_as(victim_acct)
        cli.follow(plamenu_user.username, victim_acct)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to the victim to be accepted",
        )
        victim_id = plamenu_api.lookup(victim_acct)["id"]
        assert plamenu_api.relationship(victim_id)["following"] is True

    with step("destroy ONLY Mastodon's follower row (Plamenu keeps the ghost)"):
        # `.destroy` clears Mastodon's cached followers hash, so the next
        # Collection-Synchronization header carries a fresh (empty) digest.
        mastodon.remove_follower(plamenu_user.acct, victim)

    with step(
        "the victim sends a PRIVATE mention so the delivery carries the sync header"
    ):
        victim_api.post_status(
            f"@{plamenu_user.acct} sync repair {marker}", visibility="private"
        )

    with step("Plamenu fetches the empty partial collection and drops the ghost"):
        wait_for(
            lambda: not plamenu_api.relationship(victim_id)["following"],
            desc="Plamenu to remove the ghost follow after synchronization",
        )
