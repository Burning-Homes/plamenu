"""Posts federate in both directions between Plamenu and Mastodon."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_post_federates_from_mastodon"
)
def test_post_federates_to_mastodon(alice, plamenu_user, cli, marker):
    """Covers: Create activity fan-out to remote followers and the outbound
    delivery queue — a Plamenu post appears on a Mastodon follower's home
    timeline."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post from Plamenu and watch alice's home timeline"):
        cli.post(plamenu_user.username, f"Hello from Plamenu! {marker}")
        wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post to appear on alice's home timeline",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_post_federates_to_mastodon"
)
def test_post_federates_from_mastodon(alice, plamenu_user, cli, db, marker):
    """Covers: outbound Follow auto-Accept (pending must become false) and
    inbound Create handling — a Mastodon post lands in Plamenu's statuses
    table."""
    with step(f"@{plamenu_user.username} follows {config.ALICE} (outbound Follow)"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts; the status must reach Plamenu's inbox"):
        alice.post_status(f"Hello from Mastodon! {marker}")
        wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )


@pytest.mark.federation(direction="both")
def test_content_warning_federates_both_ways(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: spoiler_text/sensitive/language on the wire in both directions
    — a Plamenu CW arrives as a Mastodon spoiler (summary + sensitive +
    contentMap), and a Mastodon CW lands in Plamenu's statuses row."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post with a CW from Plamenu; alice sees the spoiler"):
        plamenu_api.post_status(
            f"behind the warning {marker}",
            spoiler_text="e2e spoiler",
            language="en",
        )
        status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the CW'd Plamenu post to appear on alice's home timeline",
        )
        assert status["spoiler_text"] == "e2e spoiler"
        assert status["sensitive"] is True, "a CW must force sensitive on"
        assert status["language"] == "en"

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts with a CW; Plamenu stores spoiler and sensitive"):
        alice.post_status(f"masto warning body {marker}b", spoiler_text="masto spoiler")
        status_id = wait_for(
            lambda: db.status_id_containing(f"{marker}b"),
            desc="the CW'd Mastodon post to arrive in plamenu's statuses table",
        )
        spoiler, sensitive, _language = db.status_cw(status_id)
        assert spoiler == "masto spoiler"
        assert sensitive is True
