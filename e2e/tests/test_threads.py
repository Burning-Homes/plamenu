"""Thread backfill: a reply to an unseen thread pulls in its ancestors."""

from plamenu_e2e import mastodon, unique
from plamenu_e2e.steps import step, wait_for


def test_reply_backfills_unseen_thread(plamenu_user, plamenu_api, db, marker):
    """Covers: inbound reply whose parents Plamenu has never seen — the
    ancestor chain is fetched from Mastodon (thread backfill), stored
    threaded, and the conversation is whole through /context.

    Uses a fresh Mastodon author instead of alice: alice accumulates
    Plamenu followers as the suite runs, and any follower means Mastodon
    pushes her posts to Plamenu immediately — there would be nothing left
    to backfill."""
    with step("a fresh Mastodon user starts a thread Plamenu does not see"):
        username = unique("thrd")
        mastodon.create_account(username)
        author = mastodon.api_as(f"{username}@mastodon.local")
        root = author.post_status(f"thread root {marker}root")
        middle = author.post_status(
            f"thread middle {marker}mid", in_reply_to_id=root["id"]
        )
        assert db.status_id_containing(marker) is None, (
            "Plamenu must not know the thread before the mention arrives"
        )

    with step(f"the author replies deeper, mentioning @{plamenu_user.acct}"):
        author.post_status(
            f"@{plamenu_user.acct} thread leaf {marker}leaf",
            in_reply_to_id=middle["id"],
        )
        leaf_id = wait_for(
            lambda: db.status_id_containing(f"{marker}leaf"),
            desc="the mention reply to arrive in plamenu's statuses table",
        )

    with step("the unseen ancestors were backfilled and threaded"):
        mid_id = db.status_id_containing(f"{marker}mid")
        root_id = db.status_id_containing(f"{marker}root")
        assert mid_id and root_id, "the ancestors must be in the statuses table"
        assert db.status_parent_id(leaf_id) == mid_id
        assert db.status_parent_id(mid_id) == root_id
        assert db.status_parent_id(root_id) is None

    with step("the full thread is visible through /context"):
        context = plamenu_api.get(f"/api/v1/statuses/{leaf_id}/context")
        ancestors = [s["content"] for s in context["ancestors"]]
        assert len(ancestors) == 2, f"expected both ancestors, got {ancestors}"
        assert f"{marker}root" in ancestors[0]
        assert f"{marker}mid" in ancestors[1]
