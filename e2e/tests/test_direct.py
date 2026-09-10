"""Direct messages federate in both directions and land in conversations."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(direction="both")
def test_direct_messages_federate_both_ways(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Covers: outbound direct addressing (`to` = mentioned actor only —
    Mastodon must classify it as `direct`, not followers-only), inbound
    direct ingestion (visibility + conversation + unread), threading of the
    private reply into the same conversation, and the read endpoint."""
    with step(f"alice follows @{plamenu_user.acct} (strangers' DMs are filtered)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before messaging",
        )

    with step(f"@{plamenu_user.username} sends alice a private mention"):
        posted = plamenu_api.post_status(
            f"@{config.ALICE} psst, just for you {marker}", visibility="direct"
        )
        assert posted["visibility"] == "direct"
        mine = plamenu_api.conversation_containing(marker)
        assert mine, "the sender's own conversations must list the DM"
        assert mine["unread"] is False, "your own message is not unread"

    with step("alice receives it as a direct conversation on Mastodon"):
        conversation = wait_for(
            lambda: alice.conversation_containing(marker),
            desc="the DM to appear in alice's Mastodon conversations",
        )
        last = conversation["last_status"]
        assert last["visibility"] == "direct", (
            "Mastodon must read the addressing as direct, not followers-only"
        )

    with step("alice replies privately; Plamenu stores it as a direct status"):
        alice.post_status(
            f"@{plamenu_user.acct} got it {marker}r",
            visibility="direct",
            in_reply_to_id=last["id"],
        )
        status_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the Mastodon DM to arrive in plamenu's statuses table",
        )
        assert db.status_visibility(status_id) == "direct"

    with step("the reply joins the same conversation, unread, and reads away"):
        threaded = wait_for(
            lambda: plamenu_api.conversation_containing(f"{marker}r"),
            desc="the reply to show up in the plamenu user's conversations",
        )
        assert threaded["id"] == mine["id"], "a private reply must not fork the thread"
        assert threaded["unread"] is True
        assert any(
            account["acct"] == config.ALICE for account in threaded["accounts"]
        ), threaded["accounts"]
        marked = plamenu_api.read_conversation(threaded["id"])
        assert marked["unread"] is False

    with step("alice got a mention notification for the DM"):
        # Mastodon notifies private mentions as regular mentions.
        notifications = alice.get("/api/v1/notifications")
        assert any(
            n["type"] == "mention"
            and marker in ((n.get("status") or {}).get("content") or "")
            for n in notifications
        ), "alice should have a mention notification for the DM"


@pytest.mark.federation(
    direction="outbound",
    one_way_reason=(
        "wire-shape guard: Mastodon is the strict receiver (it demotes "
        "audience-only direct notes to limited); inbound audience-only DMs "
        "are already accepted by plamenu's silent-mention ingest, covered "
        "at the unit level."
    ),
)
def test_mention_less_direct_reply_reaches_mastodon(
    alice, plamenu_user, plamenu_api, db, marker
):
    """A reply composed inside a DM thread with NO typed @-mention must still
    carry `Mention` tags on the wire (the inherited audience). Mastodon
    demotes audience-only direct notes to unnotified `limited` and never files
    them under private mentions — the regression this test pins."""
    with step(f"alice DMs @{plamenu_user.acct}"):
        alice.post_status(f"@{plamenu_user.acct} ping {marker}", visibility="direct")
        wait_for(
            lambda: db.status_id_containing(marker),
            desc="alice's DM to arrive in plamenu's statuses table",
        )

    with step("the plamenu user replies without typing any mention"):
        row = wait_for(
            lambda: plamenu_api.conversation_containing(marker),
            desc="the inbound DM to open a plamenu conversation",
        )
        reply = plamenu_api.post_status(
            f"pong, no mention typed {marker}r",
            visibility="direct",
            in_reply_to_id=row["last_status"]["id"],
        )
        assert reply["visibility"] == "direct"

    with step("Mastodon files the reply as a private mention, with a notification"):
        conversation = wait_for(
            lambda: alice.conversation_containing(f"{marker}r"),
            desc="the mention-less reply to reach alice's conversations",
        )
        assert conversation["last_status"]["visibility"] == "direct", (
            "audience-only addressing was demoted — Mention tags missing on the wire"
        )
        notifications = alice.get("/api/v1/notifications")
        assert any(
            n["type"] == "mention"
            and f"{marker}r" in ((n.get("status") or {}).get("content") or "")
            for n in notifications
        ), "alice must be notified of the mention-less DM reply"
