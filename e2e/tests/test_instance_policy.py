"""Instance-level federation policy enforcement against a live Mastodon peer.

E2E: an admin suspends the `mastodon.local` domain through the
`/api/v1/admin/domain_blocks` API, and Plamenu must then refuse to federate
with it in both directions:

- Visibility/fetch cutoff: an already-known remote account becomes 404, and a
  fresh `resolve=true` webfinger of an account on the domain returns nothing
  (the live actor fetch is refused before it leaves the box).
- Delivery cutoff: a Plamenu post no longer reaches a remote follower on the
  suspended domain — its queued delivery is discarded, not retried.

Both halves are also covered deterministically by the unit/integration tests
(`crates/server/tests/instance_policy_enforcement.rs`); this test proves the
real federation path (live webfinger, a real Mastodon inbox) end to end.

The domain block is global server state, so the test always tears it back down
— a lingering `mastodon.local` suspension would break the rest of the suite.
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(direction="both")
def test_domain_suspension_cuts_off_mastodon_federation(
    plamenu_admin, alice, cli, db, marker
):
    """Covers: POST/DELETE /api/v1/admin/domain_blocks (severity=suspend), the
    live actor-fetch + cached-account visibility cutoff, and the outbound
    delivery skip — all against the real mastodon.local peer."""
    admin_user, admin_api = plamenu_admin
    masto = config.MASTODON_DOMAIN

    with step(f"alice follows @{admin_user.acct} (a real remote follower)"):
        remote = alice.resolve_account(admin_user.acct)
        assert remote, "Mastodon could not resolve the Plamenu admin"
        alice.follow(remote["id"])
        wait_for(
            lambda: alice.relationship(remote["id"])["following"],
            desc="alice's follow of the admin to be accepted",
        )

    with step("baseline: a Plamenu post reaches alice while unblocked"):
        cli.post(admin_user.username, f"before the block {marker}")
        wait_for(
            lambda: alice.home_status_containing(f"before the block {marker}"),
            desc="the pre-block post to reach alice's home timeline",
        )

    with step("Plamenu knows alice (cached via the inbound follow)"):
        alice_account = admin_api.resolve_account(config.ALICE)
        assert alice_account, "Plamenu could not resolve alice"
        alice_id = alice_account["id"]
        assert admin_api.account(alice_id), "alice must be visible before the block"
        log(f"alice is {alice_id} on plamenu")

    block_id = None
    try:
        with step(f"admin suspends {masto} via the admin API"):
            block = admin_api.post(
                "/api/v1/admin/domain_blocks", domain=masto, severity="suspend"
            )
            block_id = block["id"]
            assert block["domain"] == masto
            assert block["severity"] == "suspend"
            log(f"domain block {block_id} created")

        with step("visibility cutoff: the cached alice account is now hidden"):
            assert admin_api.account(alice_id) is None, (
                "suspended-domain account must 404"
            )

        with step("fetch cutoff: a fresh resolve of alice is refused"):
            # The live actor fetch must be denied before it leaves the box, so
            # even resolve=true returns nothing.
            assert admin_api.resolve_account(config.ALICE) is None, (
                "resolving an account on a suspended domain must return nothing"
            )

        with step("delivery cutoff: a post queued under the block is discarded"):
            cli.post(admin_user.username, f"during the block {marker}")
            # The skip happens when the delivery worker drains the job; wait
            # for the queue to clear so the lift below can't race it.
            wait_for(
                lambda: db.pending_deliveries_to(masto) == 0,
                desc="the blocked delivery to drain (skipped, not retried)",
            )
    finally:
        if block_id is not None:
            with step(f"lift the suspension on {masto}"):
                admin_api.delete(f"/api/v1/admin/domain_blocks/{block_id}")

    with step("federation resumes: a post-lift fence reaches alice"):
        cli.post(admin_user.username, f"after the block {marker}")
        wait_for(
            lambda: alice.home_status_containing(f"after the block {marker}"),
            desc="the post-lift fence to reach alice's home timeline",
        )

    with step("the during-block post never arrived"):
        # The fence (queued after the lift) has landed, so the earlier blocked
        # post — whose job was already discarded — will never show up.
        assert alice.home_status_containing(f"during the block {marker}") is None, (
            "a post made under the suspension must not have federated"
        )
