"""Boosts (`Announce`) federate in both directions with their side effects
(counts, `reblogged_by`, notifications, follower home timelines), `Undo
(Announce)` rolls them back, and an Announce of a never-seen object forces a
backfill fetch of the boosted Note."""

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_boost_federates_from_mastodon"
)
def test_boost_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: outbound `Announce` — the author's count, `reblogged_by`
    listing and `reblog` notification on Mastodon, fan-out to remote
    followers (the boost shows on alice's home timeline — of a *third*
    account's post, since Mastodon's feed dedup drops boosts of a status
    already in the feed), the local `reblogged`/`reblog` entity shape, and
    `Undo(Announce)` rolling the count back."""
    with step("create a throwaway Mastodon author (carol) and her post"):
        carol_name = unique("e2ecarol")
        mastodon.create_account(carol_name)
        carol = mastodon.api_as(f"{carol_name}@mastodon.local")
        masto_status = carol.post_status(f"boost me from plamenu {marker}")

    with step(f"alice follows @{plamenu_user.acct} (to receive the fan-out)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before boosting",
        )

    with step("plamenu resolves carol's post and boosts it"):
        local = wait_for(
            lambda: plamenu_api.resolve_status(masto_status["uri"]),
            desc="Plamenu to resolve carol's status by URL",
        )
        boost = plamenu_api.reblog(local["id"])
        assert boost["reblog"]["id"] == local["id"], (
            f"reblog entity must wrap the boosted status: {boost.get('reblog')}"
        )
        assert boost["reblog"]["reblogged"] is True, boost["reblog"]

    with step("the Announce arrives: count + listing + notification for carol"):
        wait_for(
            lambda: carol.get_status(masto_status["id"])["reblogs_count"] >= 1,
            desc="carol's reblogs_count to reach 1",
        )
        by = carol.reblogged_by(masto_status["id"])
        assert plamenu_user.acct in [a["acct"] for a in by], (
            f"reblogged_by should list the plamenu user: {[a['acct'] for a in by]}"
        )
        wait_for(
            lambda: carol.notifications_from(plamenu_user.acct, "reblog"),
            desc="a reblog notification from the plamenu user on Mastodon",
        )

    with step("the boost fans out to alice's home timeline"):
        wrapper = wait_for(
            lambda: alice.home_reblog_containing(marker),
            desc="the boost to appear on alice's home timeline",
        )
        assert wrapper["account"]["acct"] == plamenu_user.acct, wrapper["account"]

    with step("plamenu unboosts; Undo(Announce) rolls Mastodon back to 0"):
        entity = plamenu_api.unreblog(local["id"])
        assert entity["reblogged"] is False, entity
        wait_for(
            lambda: carol.get_status(masto_status["id"])["reblogs_count"] == 0,
            desc="carol's reblogs_count to drop back to 0",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_boost_federates_to_mastodon"
)
def test_boost_federates_from_mastodon(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: inbound `Announce` — a boost row + count on the boosted
    status, `reblogged_by`, the `reblog` notification, the boost on a
    follower's home timeline, and inbound `Undo(Announce)` clearing them."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("plamenu posts; alice resolves and boosts it"):
        local = plamenu_api.post_status(f"boost me from mastodon {marker}")
        masto_copy = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status by URL",
        )
        alice.reblog(masto_copy["id"])

    with step("the Announce lands: count, reblogged_by, notification, home"):
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] >= 1,
            desc="the Plamenu reblogs_count to reach 1",
        )
        by = plamenu_api.reblogged_by(local["id"])
        assert [a["acct"] for a in by] == [config.ALICE], (
            f"reblogged_by should list exactly alice: {[a['acct'] for a in by]}"
        )
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(config.ALICE, "reblog"),
            desc="a reblog notification from alice on Plamenu",
        )
        assert notifs[0]["status"]["id"] == local["id"], notifs[0]
        wrapper = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="alice's boost to appear on the follower's home timeline",
        )
        assert wrapper["account"]["acct"] == config.ALICE, wrapper["account"]
        assert db.reblog_count(int(local["id"])) == 1

    with step("alice unboosts; Undo(Announce) clears count, listing, home"):
        alice.unreblog(masto_copy["id"])
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] == 0,
            desc="the Plamenu reblogs_count to drop back to 0",
        )
        assert plamenu_api.reblogged_by(local["id"]) == []
        assert plamenu_api.home_reblog_containing(marker) is None, (
            "the retracted boost must leave the home timeline"
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound backfill: an Announce of a previously-unseen status triggers a fetch of the original; no outbound counterpart.",
)
def test_announce_of_unseen_status_backfills(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers (ordering): an inbound `Announce` whose object Plamenu has
    never seen — alice posted *before* anyone followed her, so the Create
    was never delivered — must fetch the boosted Note and thread the boost
    onto the follower's home timeline."""
    with step("alice posts while she has no plamenu followers"):
        masto_status = alice.post_status(f"unseen until boosted {marker}")
        assert db.status_id_containing(marker) is None, (
            "the fixture is broken: the status must be unknown to Plamenu"
        )

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice boosts her own earlier post; plamenu must backfill it"):
        alice.reblog(masto_status["id"])
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the boosted Note to be fetched into plamenu's statuses table",
        )
        log(f"backfilled as status {status_id}")
        wrapper = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="the boost of the backfilled Note on the home timeline",
        )
        assert wrapper["account"]["acct"] == config.ALICE, wrapper["account"]
        assert wrapper["reblog"]["account"]["acct"] == config.ALICE, (
            "the boosted Note must be attributed to its author"
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_boost_federates_from_mastodon: Undo(Announce) retracts the inbound boost notification.",
)
def test_undo_announce_retracts_the_notification(alice, plamenu_api, marker):
    """Inbound `Undo(Announce)` must also remove the `reblog` notification it
    created, as Mastodon does."""
    with step("plamenu posts; alice boosts, then unboosts"):
        local = plamenu_api.post_status(f"boost notif retraction {marker}")
        masto_copy = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status by URL",
        )
        alice.reblog(masto_copy["id"])
        wait_for(
            lambda: plamenu_api.notifications_from(config.ALICE, "reblog"),
            desc="the reblog notification to arrive",
        )
        alice.unreblog(masto_copy["id"])
        # The count reaching 0 proves the Undo was ingested first.
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] == 0,
            desc="the Undo(Announce) to be ingested (count back to 0)",
        )

    with step("the reblog notification must be gone too"):
        # Short poll on purpose: the Undo is already ingested, so a correct
        # implementation passes instantly; only the expected failure waits.
        wait_for(
            lambda: not plamenu_api.notifications_from(config.ALICE, "reblog"),
            desc="the reblog notification to be retracted",
            timeout=15,
        )
