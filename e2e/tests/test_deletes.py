"""Status deletion federates in both directions: an outbound `Delete` removes
the remote copy on Mastodon, and an inbound `Delete` removes the stored row —
along with the side effects hanging off it (boost rows, notifications)."""

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_delete_federates_from_mastodon"
)
def test_delete_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: outbound `Delete(Note)` — a federated Plamenu post vanishes
    from Mastodon (the copy 404s and leaves alice's home timeline)."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("plamenu posts; alice receives the copy"):
        local = plamenu_api.post_status(f"soon to be deleted {marker}")
        copy = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post on alice's home timeline",
        )
        log(f"alice's copy is status {copy['id']}")

    with step("plamenu deletes; the Delete removes alice's copy"):
        plamenu_api.delete_status(local["id"])
        assert plamenu_api.get_status_or_none(local["id"]) is None, (
            "the deleted status must 404 locally at once"
        )
        wait_for(
            lambda: alice.get_status_or_none(copy["id"]) is None,
            desc="alice's copy of the deleted status to 404",
        )
        assert alice.home_status_containing(marker) is None, (
            "the deleted status must leave alice's home timeline"
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_delete_federates_to_mastodon"
)
def test_delete_federates_from_mastodon(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: inbound `Delete` — the stored row disappears, a local boost of
    it is dropped with it, and the reblog notification hanging off the
    deleted status no longer renders."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts; the copy arrives and plamenu boosts it"):
        masto_status = alice.post_status(f"masto post to delete {marker}")
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )
        plamenu_api.reblog(str(status_id))
        assert db.reblog_count(status_id) == 1
        # Wait until Mastodon has ingested the Announce before deleting:
        # deleting mid-ingest races Mastodon's own reblog bookkeeping (its
        # DELETE then 422s "Duplicate record") and would flake the test.
        wait_for(
            lambda: alice.get_status(masto_status["id"])["reblogs_count"] >= 1,
            desc="Mastodon to record the boost before the delete",
        )

    with step("alice deletes; the row and the boost hanging off it vanish"):
        alice.delete_status(masto_status["id"])
        wait_for(
            lambda: db.status_id_containing(marker) is None,
            desc="the deleted status row to disappear from plamenu",
        )
        assert db.reblog_count(status_id) == 0, (
            "boost rows of a deleted status must be dropped with it"
        )
        assert plamenu_api.get_status_or_none(str(status_id)) is None
        assert plamenu_api.home_status_containing(marker) is None, (
            "the deleted status must leave the follower's home timeline"
        )


def test_delete_returns_source_text_for_redraft(plamenu_api, marker):
    """Mastodon's `DELETE /api/v1/statuses/{id}` returns the deleted status
    with its source `text` populated (and `content` omitted, like the
    `source_requested` serializer), which clients rely on for the
    delete-and-redraft flow."""
    with step("post and delete; the response must carry the source text"):
        local = plamenu_api.post_status(f"redraft me {marker}")
        deleted = plamenu_api.delete_status(local["id"])
        assert deleted.get("text") and marker in deleted["text"], (
            f"DELETE response text: {deleted.get('text')!r}"
        )
        assert "content" not in deleted, "source_requested rendering omits `content`"


@pytest.mark.federation(
    direction="inbound",
    reverse_of="test_self_destruct_broadcasts_deletions_and_gates_the_server",
)
def test_mastodon_account_delete_reaches_plamenu(plamenu_user, plamenu_api, cli, db):
    """Inbound `Delete(Actor)` (NOT suspension or Delete(Note)): a Mastodon
    account a Plamenu user follows deletes itself; the signed Delete(Actor)
    removes the remote account row on Plamenu and cascades the follow away."""
    victim = unique("victim")
    victim_acct = f"{victim}@{config.MASTODON_DOMAIN}"

    with step("create a fresh confirmed Mastodon account and let Plamenu follow it"):
        mastodon.create_account(victim)
        cli.follow(plamenu_user.username, victim_acct)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow of the victim to be accepted",
        )
        wait_for(
            lambda: db.remote_account_exists(victim, config.MASTODON_DOMAIN),
            desc="the victim's remote account row to exist on Plamenu",
        )

    with step("the account deletes itself (rails: DeleteAccountService)"):
        mastodon.delete_account(victim)

    with step("Plamenu removes the remote account and severs the relationship"):
        wait_for(
            lambda: not db.remote_account_exists(victim, config.MASTODON_DOMAIN),
            desc="the remote account row to be deleted on Plamenu",
        )
        assert plamenu_api.lookup(victim_acct) is None
        assert not db.outbound_follow_exists(
            plamenu_user.username, victim, config.MASTODON_DOMAIN
        ), "the follow of the deleted account must be gone (FK cascade)"
