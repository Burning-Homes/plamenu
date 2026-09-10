"""Self-service auto-deletion (M38): the background sweep deletes a policy's
old-enough posts through the normal federating delete path — the remote copy
vanishes from Mastodon — while excepted posts (here: pinned) survive.

The policy is inserted directly into the DB with a tiny `min_status_age`
(the settings form only offers ≥ 1 week); the sweep runs every 60 seconds.
The fixture account is unique per test, so the aggressive policy cannot
touch any other test's posts.
"""

import pytest
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound maintenance sweep: the retention cleanup deletes aged statuses and federates a Delete to remote followers; a server-lifecycle behavior with no inbound counterpart.",
)
def test_cleanup_sweep_federates_the_delete(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Covers: the statuses-cleanup sweep deleting an old post (outbound
    `Delete(Note)` reaches Mastodon) and honoring the keep-pinned default."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("plamenu posts a doomed and a pinned status; alice gets both"):
        doomed = plamenu_api.post_status(f"sweep me {marker}-doomed")
        pinned = plamenu_api.post_status(f"keep me {marker}-pinned")
        plamenu_api.post(f"/api/v1/statuses/{pinned['id']}/pin")
        doomed_copy = wait_for(
            lambda: alice.home_status_containing(f"{marker}-doomed"),
            desc="the doomed post on alice's home timeline",
        )
        wait_for(
            lambda: alice.home_status_containing(f"{marker}-pinned"),
            desc="the pinned post on alice's home timeline",
        )
        log(f"alice's doomed copy is status {doomed_copy['id']}")

    with step("an auto-delete policy (5s age, keep pinned) goes live"):
        db.enable_statuses_cleanup(plamenu_user.username, 5)

    with step("the sweep deletes the old post locally and on Mastodon"):
        wait_for(
            lambda: plamenu_api.get_status_or_none(doomed["id"]) is None,
            desc="the sweep to delete the doomed post locally (runs every 60s)",
        )
        wait_for(
            lambda: alice.get_status_or_none(doomed_copy["id"]) is None,
            desc="alice's copy of the swept post to 404",
        )
        assert plamenu_api.get_status_or_none(pinned["id"]) is not None, (
            "the pinned post must survive the sweep (keep_pinned default)"
        )
