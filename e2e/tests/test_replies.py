"""Replies and mentions federate in both directions with their side effects:
`inReplyTo` threading, `mention` notifications, the parent's `replies_count`,
and `/context` on both ends. (Backfill of unseen ancestors is covered by
test_threads.py; this module covers the ordinary reply flow.)"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_reply_and_mention_federate_from_mastodon"
)
def test_reply_and_mention_federate_to_mastodon(
    alice, plamenu_user, plamenu_api, marker
):
    """Covers: an outbound reply — the `Create(Note)` with `inReplyTo` +
    `Mention` tag delivered to the parent author (no follower relationship
    involved), alice's `mention` notification, her `replies_count`, and the
    reply threading into her `/context` descendants."""
    with step("alice posts; plamenu resolves the status by URL"):
        masto_status = alice.post_status(f"reply to me from plamenu {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(masto_status["uri"]),
            desc="Plamenu to resolve alice's status by URL",
        )

    with step("plamenu replies, mentioning alice"):
        reply = plamenu_api.post_status(
            f"@{config.ALICE} a reply from plamenu {marker}r",
            in_reply_to_id=local["id"],
        )
        assert reply["in_reply_to_id"] == local["id"], reply
        mentioned = [m["acct"] for m in reply["mentions"]]
        assert config.ALICE in mentioned, f"reply mentions lack alice: {mentioned}"

    with step("alice gets a mention notification threaded onto her post"):
        notifs = wait_for(
            lambda: alice.notifications_from(plamenu_user.acct, "mention"),
            desc="a mention notification from the plamenu user on Mastodon",
        )
        arrived = notifs[0]["status"]
        assert arrived["in_reply_to_id"] == masto_status["id"], (
            f"the reply must thread under alice's post: {arrived['in_reply_to_id']}"
        )

    with step("her replies_count and /context reflect the reply"):
        wait_for(
            lambda: alice.get_status(masto_status["id"])["replies_count"] >= 1,
            desc="alice's replies_count to reach 1",
        )
        descendants = alice.context(masto_status["id"])["descendants"]
        assert any(f"{marker}r" in s["content"] for s in descendants), (
            f"context descendants lack the reply: {[s['content'] for s in descendants]}"
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_reply_and_mention_federate_to_mastodon"
)
def test_reply_and_mention_federate_from_mastodon(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Covers: an inbound reply — `in_reply_to_id` linking on the stored row,
    the `mention` notification for the parent author, the parent's
    `replies_count`, and `/context` listing the descendant."""
    with step("plamenu posts; alice resolves the status by URL"):
        local = plamenu_api.post_status(f"reply to me from mastodon {marker}")
        masto_copy = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status by URL",
        )

    with step("alice replies, mentioning the plamenu author"):
        alice.post_status(
            f"@{plamenu_user.acct} a reply from mastodon {marker}r",
            in_reply_to_id=masto_copy["id"],
        )

    with step("the reply arrives threaded under the plamenu post"):
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the Mastodon reply to arrive in plamenu's statuses table",
        )
        assert db.status_parent_id(reply_id) == int(local["id"]), (
            f"in_reply_to_id must point at the parent: {db.status_parent_id(reply_id)}"
        )
        log(f"reply stored as status {reply_id}")

    with step("the author gets a mention notification carrying the reply"):
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(config.ALICE, "mention"),
            desc="a mention notification from alice on Plamenu",
        )
        assert notifs[0]["status"]["id"] == str(reply_id), notifs[0]["status"]["id"]
        mentioned = [m["acct"] for m in notifs[0]["status"]["mentions"]]
        assert plamenu_user.username in mentioned, (
            f"the reply's mentions lack the author: {mentioned}"
        )

    with step("replies_count and /context reflect the descendant"):
        assert plamenu_api.get_status(local["id"])["replies_count"] >= 1
        descendants = plamenu_api.context(local["id"])["descendants"]
        assert any(f"{marker}r" in s["content"] for s in descendants), (
            f"context descendants lack the reply: {[s['content'] for s in descendants]}"
        )
