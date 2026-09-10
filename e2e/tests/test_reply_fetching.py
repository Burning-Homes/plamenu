"""Opening a remote thread pulls in replies over the ActivityPub `replies`
collection — replies Plamenu never received via federation, from an account it
doesn't follow. Mirrors Mastodon's context-open fetch-all-replies job; the
serving direction (Mastodon backfilling replies we advertise) lives in
test_collections.py.

The thread author is a *fresh* Mastodon account with no Plamenu followers, so
Mastodon never delivers the posts to Plamenu — anything that shows up did so
because we fetched it, not because it was pushed to us. (alice can't be used:
accumulated test runs leave her with many Plamenu followers, so her public
self-replies would federate on their own.)"""

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import log, step, wait_for


@pytest.fixture(scope="module")
def stranger():
    """A confirmed Mastodon account nobody on Plamenu follows, with an API
    client. Created once per module (tootctl is slow)."""
    username = unique("stranger")
    mastodon.create_account(username)
    return mastodon.api_as(f"{username}@mastodon.local")


def test_opening_remote_thread_fetches_undelivered_replies(
    stranger, plamenu_api, db, marker
):
    """Covers: a Plamenu user opening a remote thread triggers a crawl of the
    origin's `replies` collection, ingesting a direct reply and a reply-to-the-
    reply (recursion) that were never delivered to Plamenu, threaded under the
    root and visible in `/context`."""
    with step("the stranger builds a thread on Mastodon, none of it federated to us"):
        root = stranger.post_status(f"reply-fetch root {marker}root")
        reply1 = stranger.post_status(
            f"reply-fetch first {marker}reply1", in_reply_to_id=root["id"]
        )
        stranger.post_status(
            f"reply-fetch second {marker}reply2", in_reply_to_id=reply1["id"]
        )

    with step("plamenu resolves only the root by URL"):
        local_root = wait_for(
            lambda: plamenu_api.resolve_status(root["uri"]),
            desc="Plamenu to resolve the root status by URL",
        )
        # No Plamenu account follows the stranger, so the replies were never
        # delivered — only the root is known so far.
        assert db.status_id_containing(f"{marker}reply1") is None, (
            "reply1 must not be present before the thread is opened"
        )
        assert db.status_id_containing(f"{marker}reply2") is None, (
            "reply2 must not be present before the thread is opened"
        )

    with step("opening the thread's context crawls the replies collection"):
        # The first /context call enqueues the crawl and returns what we have;
        # the worker fetches the replies in the background, so we poll.
        def replies_in_context():
            descendants = plamenu_api.context(local_root["id"])["descendants"]
            contents = [s["content"] for s in descendants]
            have_first = any(f"{marker}reply1" in c for c in contents)
            have_second = any(f"{marker}reply2" in c for c in contents)
            return descendants if have_first and have_second else None

        descendants = wait_for(
            replies_in_context,
            desc="Plamenu to fetch the undelivered replies into /context",
        )
        log(f"context descendants after crawl: {len(descendants)}")

    with step("the fetched replies are threaded correctly"):
        reply1_id = db.status_id_containing(f"{marker}reply1")
        reply2_id = db.status_id_containing(f"{marker}reply2")
        assert db.status_parent_id(reply1_id) == int(local_root["id"]), (
            "the direct reply must thread under the root"
        )
        assert db.status_parent_id(reply2_id) == reply1_id, (
            "the reply-to-the-reply must thread under the first reply (recursion)"
        )
        # Authored by the stranger, whom Plamenu does not follow.
        authors = {s["account"]["acct"] for s in descendants}
        assert any(a.endswith(config.MASTODON_DOMAIN) for a in authors), authors
