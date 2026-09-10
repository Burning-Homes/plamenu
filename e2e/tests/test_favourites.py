"""Favourites (`Like`) federate in both directions, with their side effects:
counts, `favourited_by` listings, notifications — and `Undo(Like)` rolls all
of it back."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_favourite_federates_from_mastodon"
)
def test_favourite_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: outbound `Like` — a Plamenu favourite raises the count and
    lands a `favourite` notification on Mastodon; `/favourites` and the
    `favourited` flag track it locally; `Undo(Like)` rolls the count back."""
    with step("alice posts; plamenu resolves the status by URL"):
        masto_status = alice.post_status(f"fav me from plamenu {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(masto_status["uri"]),
            desc="Plamenu to resolve alice's status by URL",
        )
        log(f"resolved to Plamenu status id {local['id']}")

    with step("plamenu favourites it; the local flags flip immediately"):
        entity = plamenu_api.favourite(local["id"])
        assert entity["favourited"] is True, entity
        assert entity["favourites_count"] >= 1, entity
        faved = plamenu_api.get("/api/v1/favourites")
        assert any(s["id"] == local["id"] for s in faved), (
            f"/favourites lacks the status: {[s['id'] for s in faved]}"
        )

    with step("the Like arrives: count + favourite notification on Mastodon"):
        wait_for(
            lambda: alice.get_status(masto_status["id"])["favourites_count"] >= 1,
            desc="alice's favourites_count to reach 1",
        )
        wait_for(
            lambda: alice.notifications_from(plamenu_user.acct, "favourite"),
            desc="a favourite notification from the plamenu user on Mastodon",
        )

    with step("plamenu unfavourites; Undo(Like) rolls Mastodon back to 0"):
        entity = plamenu_api.unfavourite(local["id"])
        assert entity["favourited"] is False, entity
        assert not any(
            s["id"] == local["id"] for s in plamenu_api.get("/api/v1/favourites")
        ), "/favourites still lists the unfavourited status"
        wait_for(
            lambda: alice.get_status(masto_status["id"])["favourites_count"] == 0,
            desc="alice's favourites_count to drop back to 0",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_favourite_federates_to_mastodon"
)
def test_favourite_federates_from_mastodon(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Covers: inbound `Like` — count, `favourited_by` listing and the
    `favourite` notification on the Plamenu author; inbound `Undo(Like)`
    clears all three."""
    with step("plamenu posts; alice resolves the status by URL"):
        local = plamenu_api.post_status(f"fav me from mastodon {marker}")
        masto_copy = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status by URL",
        )

    with step("alice favourites it; count + listing + notification on Plamenu"):
        alice.favourite(masto_copy["id"])
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] >= 1,
            desc="the Plamenu favourites_count to reach 1",
        )
        by = plamenu_api.favourited_by(local["id"])
        assert [a["acct"] for a in by] == [config.ALICE], (
            f"favourited_by should list exactly alice: {[a['acct'] for a in by]}"
        )
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(config.ALICE, "favourite"),
            desc="a favourite notification from alice on Plamenu",
        )
        assert notifs[0]["status"]["id"] == local["id"], notifs[0]
        assert db.notification_count(plamenu_user.username, "favourite") == 1

    with step("alice unfavourites; Undo(Like) clears count, listing, notification"):
        alice.unfavourite(masto_copy["id"])
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] == 0,
            desc="the Plamenu favourites_count to drop back to 0",
        )
        assert plamenu_api.favourited_by(local["id"]) == []


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_favourite_federates_from_mastodon: Undo(Like) retracts the inbound favourite notification.",
)
def test_undo_like_retracts_the_notification(alice, plamenu_api, marker):
    """Inbound `Undo(Like)` must also remove the `favourite` notification it
    created, as Mastodon does."""
    with step("plamenu posts; alice favourites, then unfavourites"):
        local = plamenu_api.post_status(f"notif retraction {marker}")
        masto_copy = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status by URL",
        )
        alice.favourite(masto_copy["id"])
        wait_for(
            lambda: plamenu_api.notifications_from(config.ALICE, "favourite"),
            desc="the favourite notification to arrive",
        )
        alice.unfavourite(masto_copy["id"])
        # The count reaching 0 proves the Undo was ingested before we look
        # at the notification list.
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] == 0,
            desc="the Undo(Like) to be ingested (count back to 0)",
        )

    with step("the favourite notification must be gone too"):
        # Short poll on purpose: the Undo is already ingested, so a correct
        # implementation passes instantly; only the expected failure waits.
        wait_for(
            lambda: not plamenu_api.notifications_from(config.ALICE, "favourite"),
            desc="the favourite notification to be retracted",
            timeout=15,
        )
