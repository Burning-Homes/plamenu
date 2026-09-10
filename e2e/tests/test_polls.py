"""Polls federate in both directions between Plamenu and Mastodon."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_poll_federates_from_mastodon"
)
def test_poll_federates_to_mastodon(alice, plamenu_user, plamenu_api, cli, db, marker):
    """Covers: outbound Create(Question) with options, a federated inbound
    vote (Create(Note) with `name`) tallying on the local poll, and the
    resulting Update(Question) fan-out refreshing Mastodon's copy."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post a poll from Plamenu; alice sees a poll with our options"):
        entity = plamenu_api.post_poll(f"pick one {marker}", ["tea", "coffee"])
        local_poll_id = entity["poll"]["id"]
        assert [o["title"] for o in entity["poll"]["options"]] == ["tea", "coffee"]
        remote_status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu poll to appear on alice's home timeline",
        )
        remote_poll = remote_status["poll"]
        assert remote_poll, "Mastodon must render the Question as a poll"
        assert [o["title"] for o in remote_poll["options"]] == ["tea", "coffee"]
        assert remote_poll["multiple"] is False
        assert remote_poll["expired"] is False

    with step("alice votes; the vote lands on Plamenu's poll"):
        alice.vote(remote_poll["id"], [0])
        wait_for(
            lambda: db.poll_tallies(int(local_poll_id)) == [1, 0],
            desc="alice's federated vote to tally on the Plamenu poll",
        )
        rendered = plamenu_api.get_poll(local_poll_id)
        assert rendered["votes_count"] == 1
        assert rendered["voters_count"] == 1

    with step("the tally Update(Question) reaches Mastodon"):
        wait_for(
            lambda: alice.get_poll(remote_poll["id"])["votes_count"] == 1,
            desc="Mastodon's copy of the poll to show the refreshed tally",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_poll_federates_to_mastodon"
)
def test_poll_federates_from_mastodon(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: inbound Create(Question) producing a poll row and a rendered
    poll entity, and an outbound federated vote that Mastodon's origin poll
    accepts and tallies."""
    with step(f"@{plamenu_user.username} follows {config.ALICE} (outbound Follow)"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts a poll; Plamenu stores poll options and tallies"):
        remote_status = alice.post_poll(f"choose {marker}", ["red", "blue"])
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon poll to arrive in plamenu's statuses table",
        )
        stored = wait_for(
            lambda: db.poll_for_status(status_id),
            desc="the poll row for the inbound Question",
        )
        poll_id, options, tallies = stored
        assert options == ["red", "blue"]
        assert tallies == [0, 0]

    with step("the poll renders through Plamenu's client API"):
        rendered = plamenu_api.get_poll(str(poll_id))
        assert [o["title"] for o in rendered["options"]] == ["red", "blue"]
        assert rendered["expired"] is False
        assert rendered["voted"] is False

    with step(f"@{plamenu_user.username} votes; the origin poll tallies it"):
        voted = plamenu_api.vote(str(poll_id), [1])
        assert voted["own_votes"] == [1]
        assert voted["voted"] is True
        wait_for(
            lambda: alice.get_poll(remote_status["poll"]["id"])["votes_count"] == 1,
            desc="the federated vote to tally on Mastodon's origin poll",
        )
        origin = alice.get_poll(remote_status["poll"]["id"])
        assert origin["options"][1]["votes_count"] == 1
