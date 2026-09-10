"""Block federation in both directions.

Outbound: a Plamenu user blocks alice@mastodon.local — the mutual follows
must be severed on both instances (Undo(Follow) + Reject(Follow)), the Block
must be delivered, re-follow attempts from the blocked side must bounce, and
an unblock (Undo(Block)) must let alice follow again.

Inbound: alice blocks the Plamenu user — her Block lands in our inbox, the
block row and severed follows must appear in Plamenu's database, the blocked
user's follow attempt must be a local 403, and her Undo(Block) must lift it.
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for


def _mutual_follow(plamenu_user, plamenu_api, alice, cli):
    """Wires the mutual follow both tests start from; returns
    (alice's id on Plamenu, the Plamenu user's id on Mastodon)."""
    account = plamenu_api.lookup(config.ALICE)
    if account is None:
        cli.post(plamenu_user.username, f"wiring up @{config.ALICE} for an e2e run")
        account = wait_for(
            lambda: plamenu_api.lookup(config.ALICE),
            desc="the mention to seed alice into plamenu's accounts",
        )
    alice_id = account["id"]
    plamenu_api.follow(alice_id)
    wait_for(
        lambda: plamenu_api.relationship(alice_id)["following"],
        desc="plamenu user's follow of alice to be accepted",
    )
    remote = alice.resolve_account(plamenu_user.acct)
    assert remote, "Mastodon could not resolve our user"
    remote_id = remote["id"]
    alice.follow(remote_id)
    wait_for(
        lambda: alice.relationship(remote_id)["following"],
        desc="alice's follow of the plamenu user to be accepted",
    )
    log(f"mutual follow up: alice={alice_id} on plamenu, user={remote_id} on mastodon")
    return alice_id, remote_id


@pytest.mark.federation(
    direction="outbound", reverse_of="test_block_federates_from_mastodon"
)
def test_block_federates_to_mastodon(plamenu_user, plamenu_api, alice, cli, db):
    """Covers: POST /accounts/{id}/block + /unblock, GET /api/v1/blocks,
    relationship flags, follow severing in both directions (outbound
    Undo(Follow) + Reject(Follow) observed by Mastodon), outbound Block
    (Mastodon refuses re-follows of the blocker), outbound Undo(Block)."""
    alice_id, remote_id = _mutual_follow(plamenu_user, plamenu_api, alice, cli)

    with step("block alice through the client API"):
        rel = plamenu_api.block(alice_id)
        assert rel["blocking"] is True
        assert rel["following"] is False
        assert rel["followed_by"] is False
        assert [b["acct"] for b in plamenu_api.blocks()] == [config.ALICE], (
            "alice should appear in /api/v1/blocks"
        )
        assert db.follower_count(plamenu_user.username) == 0, (
            "alice's follow row must be severed immediately"
        )

    with step("Mastodon observes the severing (Undo(Follow) + Reject(Follow))"):
        wait_for(
            lambda: not alice.relationship(remote_id)["following"],
            desc="alice's follow to be rejected (Reject(Follow) processed)",
        )
        wait_for(
            lambda: not alice.relationship(remote_id)["followed_by"],
            desc="our follow of alice to disappear (Undo(Follow) processed)",
        )

    with step("a re-follow from alice bounces off the block"):
        # Once Mastodon processes our Block it refuses the follow locally
        # with a 403; an attempt that slips in before that is auto-rejected
        # by Plamenu. Either way no follow may survive.
        def follow_refused() -> bool:
            try:
                alice.follow(remote_id)
            except ApiError as exc:
                return "403" in str(exc)
            return False

        wait_for(
            follow_refused,
            desc="Mastodon to refuse alice's follow (our Block processed)",
        )

        def follow_absent() -> bool:
            rel = alice.relationship(remote_id)
            return not rel["following"] and not rel["requested"]

        wait_for(
            follow_absent,
            desc="any slipped-in follow attempt to be rejected",
        )
        assert db.follower_count(plamenu_user.username) == 0, (
            "the rejected follow must not leave a row"
        )

    with step("unblock: Undo(Block) federates, alice can follow again"):
        rel = plamenu_api.unblock(alice_id)
        assert rel["blocking"] is False
        assert plamenu_api.blocks() == []

        # Mastodon keeps refusing until it processes the Undo(Block).
        def follow_allowed() -> bool:
            try:
                alice.follow(remote_id)
            except ApiError:
                return False
            return True

        wait_for(
            follow_allowed,
            desc="Mastodon to allow alice's follow again (Undo(Block) processed)",
        )
        wait_for(
            lambda: alice.relationship(remote_id)["following"],
            desc="alice's follow to be accepted after the unblock",
        )
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="the follow row to reappear in plamenu's db",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_block_federates_to_mastodon"
)
def test_block_federates_from_mastodon(plamenu_user, plamenu_api, alice, cli, db):
    """Covers: inbound Block (block row + severed follows), blocked_by
    relationship flag, 403 on following the blocker, inbound Undo(Block)."""
    alice_id, remote_id = _mutual_follow(plamenu_user, plamenu_api, alice, cli)

    with step("alice blocks the plamenu user on Mastodon"):
        alice.block(remote_id)
        wait_for(
            lambda: db.inbound_block_count(plamenu_user.username) == 1,
            desc="alice's Block to land in plamenu's blocks table",
        )
        assert db.follower_count(plamenu_user.username) == 0, (
            "the inbound Block must sever alice's follow row"
        )
        assert db.outbound_follow_pending(plamenu_user.username) is None, (
            "the inbound Block must sever our follow of alice"
        )
        rel = plamenu_api.relationship(alice_id)
        assert rel["blocked_by"] is True
        assert rel["following"] is False

    with (
        step("following the blocker is forbidden locally"),
        pytest.raises(ApiError, match="403"),
    ):
        plamenu_api.follow(alice_id)

    with step("alice unblocks: Undo(Block) lifts it"):
        alice.unblock(remote_id)
        wait_for(
            lambda: db.inbound_block_count(plamenu_user.username) == 0,
            desc="the Undo(Block) to remove the block row",
        )
        assert plamenu_api.relationship(alice_id)["blocked_by"] is False
        plamenu_api.follow(alice_id)
        wait_for(
            lambda: plamenu_api.relationship(alice_id)["following"],
            desc="the re-follow of alice to be accepted after her unblock",
        )
