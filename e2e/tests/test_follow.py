"""Follow federation at the ActivityPub level, driven from the Mastodon side.

A real Mastodon instance discovers a Plamenu user (webfinger + actor fetch),
follows it (signed Follow into our inbox, signed Accept back out), and
unfollows it (Undo(Follow) must remove the row from Plamenu's database).
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import Api
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="inbound", reverse_of="test_follow_lifecycle_through_client_api"
)
def test_mastodon_follows_and_unfollows_plamenu_user(alice, plamenu_user, db):
    """Covers: webfinger + actor fetch by Mastodon, HTTP-signature verification
    both ways, inbound Follow -> outbound Accept, follows row lifecycle,
    followers listings (client API + AP collection), inbound Undo(Follow)."""
    with step(f"resolve @{plamenu_user.acct} from Mastodon (webfinger + actor fetch)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        log(f"resolved to Mastodon account id {account['id']}")

    with step("follow: signed Follow -> plamenu inbox -> signed Accept -> Mastodon"):
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="Mastodon relationship to become following=true (our Accept processed)",
        )
        assert db.follower_count(plamenu_user.username) == 1, (
            "expected exactly one follow row in plamenu's db"
        )

    with step("alice appears in the followers listings (client API + AP collection)"):
        plamenu = Api(config.PLAMENU_URL)
        local = plamenu.lookup(plamenu_user.username)
        followers = plamenu.followers(local["id"])
        assert [f["acct"] for f in followers] == [config.ALICE], (
            f"client-API followers list should be exactly [alice], got {followers!r}"
        )

        collection = plamenu.ap_get(f"/users/{plamenu_user.username}/followers")
        assert collection["type"] == "OrderedCollection"
        assert collection["totalItems"] == 1
        page = plamenu.ap_get(f"/users/{plamenu_user.username}/followers?page=1")
        alice_actor = followers[0]["uri"]
        assert config.MASTODON_DOMAIN in alice_actor
        assert page["orderedItems"] == [alice_actor], (
            f"AP followers page should list alice's actor, got {page['orderedItems']!r}"
        )

    with step("unfollow: Undo(Follow) -> plamenu inbox"):
        alice.unfollow(account["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the follow row to disappear from plamenu's db after Undo",
        )
