"""Report (`Flag`) federation in both directions.

Outbound: a Plamenu user reports alice@mastodon.local with `forward: true` —
Plamenu must deliver a `Flag` (signed by its instance actor) to Mastodon,
which turns it into a moderation report about alice.

Inbound: alice reports the Plamenu user with `forward: true` — Mastodon
forwards a `Flag` to Plamenu's inbox, which must land as a row in Plamenu's
`reports` table targeting the local user.
"""

import pytest
from plamenu_e2e import config, mastodon
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_report_federates_from_mastodon"
)
def test_report_federates_to_mastodon(plamenu_api, marker):
    """Covers: POST /api/v1/reports for a remote target, the Report entity,
    and the outbound instance-actor-signed Flag arriving as a Mastodon
    report."""
    comment = f"e2e report to mastodon {marker}"

    with step("resolve alice into Plamenu"):
        account = plamenu_api.resolve_account(config.ALICE)
        assert account, "Plamenu could not resolve alice"
        alice_id = account["id"]
        log(f"alice is {alice_id} on plamenu")

    with step("report alice through the client API, forwarded"):
        report = plamenu_api.report(
            alice_id, comment=comment, category="spam", forward=True
        )
        assert report["category"] == "spam"
        assert report["comment"] == comment
        assert report["forwarded"] is True
        assert report["action_taken"] is False
        assert report["target_account"]["acct"] == config.ALICE

    with step("Mastodon turns our forwarded Flag into a report about alice"):
        wait_for(
            lambda: mastodon.report_count_about("alice", marker) >= 1,
            desc="our forwarded Flag to become a report on Mastodon",
            # Each poll is a (~10s) rails-runner; poll gently.
            interval=5.0,
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_report_federates_to_mastodon"
)
def test_report_federates_from_mastodon(plamenu_user, plamenu_api, alice, db, marker):
    """Covers: inbound Flag from Mastodon (alice's forwarded report) stored
    against the local user in Plamenu's `reports` table."""
    comment = f"e2e report from mastodon {marker}"

    with step("alice resolves the Plamenu user"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve the Plamenu user"
        remote_id = remote["id"]
        log(f"plamenu user is {remote_id} on mastodon")

    with step("alice reports the Plamenu user on Mastodon, forwarded"):
        alice.report(remote_id, comment=comment, category="spam", forward=True)

    with step("the forwarded Flag lands in Plamenu's reports table"):
        wait_for(
            lambda: db.inbound_report_count(plamenu_user.username, marker) == 1,
            desc="alice's forwarded report to be stored in plamenu",
        )
