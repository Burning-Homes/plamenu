"""Per-follow relationship settings (M32) across federation.

A Plamenu user follows alice@mastodon.local with notify-on-new-posts on and
boosts hidden. Alice's next post must raise a `status` notification at
ingest, her boosts must stay off the home timeline even once the Announce is
stored, and turning notify off must silence later posts.
"""

import uuid

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="per-follow settings (M32) applied to a followed account's inbound Create/Announce (status-notify, hide-boosts gate); a receive-side behavior with no outbound counterpart.",
)
def test_follow_settings_across_federation(plamenu_user, plamenu_api, alice, cli, db):
    """Covers: POST /accounts/{id}/follow with reblogs/notify params, real
    showing_reblogs/notifying in the Relationship entity, the `status`
    notification on inbound Create, and the hide-boosts home-timeline gate
    on an inbound Announce."""
    with step(f"ensure {config.ALICE} is known to Plamenu"):
        account = plamenu_api.lookup(config.ALICE)
        if account is None:
            cli.post(plamenu_user.username, f"wiring up @{config.ALICE} for an e2e run")
            account = wait_for(
                lambda: plamenu_api.lookup(config.ALICE),
                desc="the mention to seed alice into plamenu's accounts",
            )
        alice_id = account["id"]
        log(f"alice is plamenu account id {alice_id}")

    with step("follow alice with notify on and boosts hidden"):
        rel = plamenu_api.post(
            f"/api/v1/accounts/{alice_id}/follow",
            notify="true",
            reblogs="false",
        )
        assert rel["notifying"] is True, f"notifying should be true, got {rel!r}"
        assert rel["showing_reblogs"] is False, (
            f"showing_reblogs should be false, got {rel!r}"
        )
        wait_for(
            lambda: plamenu_api.relationship(alice_id)["following"],
            desc="plamenu relationship to become following=true (Accept processed)",
        )

    marker_notify = f"m32-notify-{uuid.uuid4().hex[:8]}"
    with step("alice posts; plamenu raises a `status` notification at ingest"):
        alice.post_status(f"fresh off the presses {marker_notify}")
        notification = wait_for(
            lambda: next(
                (
                    n
                    for n in plamenu_api.notifications_from(config.ALICE, "status")
                    if marker_notify in n["status"]["content"]
                ),
                None,
            ),
            desc="a `status` notification for alice's new post",
        )
        assert notification["status"]["account"]["acct"] == config.ALICE

    marker_boost = f"m32-boost-{uuid.uuid4().hex[:8]}"
    with step("alice boosts a post; the stored Announce stays off home"):
        boosted = alice.post_status(f"boost fodder {marker_boost}")
        alice.post(f"/api/v1/statuses/{boosted['id']}/reblog")
        original_id = wait_for(
            lambda: db.status_id_containing(marker_boost),
            desc="alice's post to arrive at plamenu",
        )
        wait_for(
            lambda: db.reblog_count(original_id) > 0,
            desc="the Announce to be stored as a boost row",
        )
        assert plamenu_api.home_reblog_containing(marker_boost) is None, (
            "a hidden-boosts follow leaked a boost onto the home timeline"
        )
        # The original itself still flows in normally.
        assert plamenu_api.home_status_containing(marker_boost) is not None, (
            "alice's own post should still reach the home timeline"
        )

    marker_quiet = f"m32-quiet-{uuid.uuid4().hex[:8]}"
    with step("turning notify off silences later posts"):
        rel = plamenu_api.post(f"/api/v1/accounts/{alice_id}/follow", notify="false")
        assert rel["notifying"] is False
        assert rel["showing_reblogs"] is False, "other settings must be untouched"
        alice.post_status(f"nothing to see here {marker_quiet}")
        wait_for(
            lambda: plamenu_api.home_status_containing(marker_quiet),
            desc="the quiet post to reach the home timeline",
        )
        assert not any(
            marker_quiet in n["status"]["content"]
            for n in plamenu_api.notifications_from(config.ALICE, "status")
        ), "notify=false must not raise `status` notifications"

    with step("cleanup: unfollow alice"):
        plamenu_api.unfollow(alice_id)
