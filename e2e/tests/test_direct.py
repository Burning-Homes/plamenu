"""Direct messages federate in both directions and land in conversations."""

import pytest
from plamenu_e2e import config, mastodon, plamenu, unique
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
    direction="inbound",
    one_way_reason=(
        "recipient-authorized fetch: a direct remote object can only be "
        "dereferenced by its addressed local recipient"
    ),
)
def test_recipient_signed_fetch_resolves_uncached_direct_status(
    plamenu_user, plamenu_api, cli, db, marker
):
    """An addressed local user, but not another user, can refetch a DM."""
    with step("a fresh Mastodon author sends the local user a direct status"):
        username = unique("authdirect")
        mastodon.create_account(username)
        author = mastodon.api_as(f"{username}@mastodon.local")
        target = author.resolve_account(plamenu_user.acct)
        assert target, "Mastodon could not resolve the direct-message recipient"
        remote = author.post_status(
            f"@{plamenu_user.acct} recipient-only {marker}", visibility="direct"
        )
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the direct status to arrive at Plamenu",
        )
        assert db.status_visibility(status_id) == "direct"

    with step("remove the delivered copy so URL resolution must dereference it"):
        status_uri = remote["uri"]
        db.forget_remote_status(status_uri)
        assert db.status_id_containing(marker) is None

    with step("an unrelated local actor cannot fetch the direct object"):
        stranger = plamenu.User()
        cli.account_add(stranger)
        stranger_api = plamenu.login(stranger)
        denied = stranger_api.search(status_uri, resolve=True, type="statuses")[
            "statuses"
        ]
        assert denied == [], f"a non-recipient resolved a direct status: {denied!r}"

    with step("the addressed recipient refetches the same direct object"):
        statuses = plamenu_api.search(status_uri, resolve=True, type="statuses")[
            "statuses"
        ]
        assert statuses and marker in statuses[0]["content"], (
            "the recipient's signed fetch did not return the direct status "
            f"(got {statuses!r})"
        )
        assert statuses[0]["visibility"] == "direct", statuses[0]
        stored_id = db.status_id_containing(marker)
        assert stored_id is not None
        assert stranger_api.get_status_or_none(str(stored_id)) is None


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
