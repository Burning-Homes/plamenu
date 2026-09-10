"""The actor outbox over federation, both directions: when Mastodon first
resolves a Plamenu account it fetches the advertised outbox and stores its
`totalItems` as the account's `statuses_count` — and Plamenu does the same
for Mastodon actors it discovers."""

import pytest
from plamenu_e2e import mastodon
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound",
    reverse_of="test_plamenu_reads_mastodon_outbox_total_as_statuses_count",
)
def test_mastodon_reads_outbox_total_as_statuses_count(
    alice, plamenu_api, plamenu_user, marker
):
    """Covers: `GET /users/{name}/outbox` (the envelope Mastodon's
    `ProcessAccountService` dereferences for `totalItems`)."""
    with step("a plamenu user with posts Mastodon never saw delivered"):
        plamenu_api.post_status(f"outbox public {marker}")
        plamenu_api.post_status(f"outbox unlisted {marker}", visibility="unlisted")
        # Followers-only posts count too (Mastodon's statuses_count counter
        # only excludes direct messages).
        plamenu_api.post_status(f"outbox private {marker}", visibility="private")

    with step("alice resolves the account by webfinger"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, f"could not resolve {plamenu_user.acct}"

    with step("the outbox totalItems became Mastodon's statuses_count"):

        def counted():
            fetched = alice.get(f"/api/v1/accounts/{account['id']}")
            return fetched if fetched["statuses_count"] == 3 else None

        wait_for(
            counted,
            desc="Mastodon to ingest the outbox totalItems as statuses_count",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_mastodon_reads_outbox_total_as_statuses_count"
)
def test_plamenu_reads_mastodon_outbox_total_as_statuses_count(plamenu_api, marker):
    """Covers: inbound actor `outbox` ingest (MC5) — discovery stores the
    collection URL and a spawned sync reads its `totalItems` into the remote
    account's rendered `statuses_count`, so the profile shows the origin's
    authoritative total instead of the locally-known slice (none here)."""
    owner = f"outboxer{marker[-8:]}"
    with step(f"a fresh Mastodon account @{owner} with posts Plamenu never saw"):
        mastodon.create_account(owner)
        owner_api = mastodon.api_as(f"{owner}@mastodon.local")
        owner_api.post_status(f"outbox public {marker}")
        owner_api.post_status(f"outbox unlisted {marker}", visibility="unlisted")
        # Followers-only posts count too (the outbox totalItems mirrors
        # Mastodon's statuses_count counter, which only excludes DMs).
        owner_api.post_status(f"outbox private {marker}", visibility="private")

    with step("Plamenu discovers the account and syncs the outbox total"):
        account = plamenu_api.resolve_account(f"{owner}@mastodon.local")
        assert account, f"Plamenu could not resolve @{owner}"

        def counted():
            fetched = plamenu_api.get(f"/api/v1/accounts/{account['id']}")
            return fetched if fetched["statuses_count"] == 3 else None

        wait_for(
            counted,
            desc="the outbox totalItems sync to land as statuses_count",
        )
