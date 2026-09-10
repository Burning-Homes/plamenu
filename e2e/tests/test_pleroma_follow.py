"""Follow federation between Plamenu and Pleroma, both directions.

Pleroma differs from Mastodon in ways that have broken Plamenu interop before
— most notably it serves its actor public key with a trailing blank line after
the PEM footer, which a strict RFC 7468 decoder rejects, so *every* signed
activity from Pleroma failed verification and inbound follows hung forever in
Pleroma's "request sent" state. These tests exercise the signed Follow/Accept
handshake against a real Pleroma instance to keep that fixed.
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import Api
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_follows_and_unfollows_pleroma_user"
)
def test_pleroma_follows_and_unfollows_plamenu_user(pleroma_bob, plamenu_user, db):
    """Inbound: Pleroma discovers a Plamenu user and follows it (signed Follow
    into our inbox -> signed Accept back). The Accept only flips Pleroma to
    `following` if Pleroma could verify our Accept *and* we could verify its
    Follow — the latter is the regression that the PEM-trailing-newline fix
    restores. Then Undo(Follow) removes the row."""
    with step(f"resolve @{plamenu_user.acct} from Pleroma (webfinger + actor fetch)"):
        account = pleroma_bob.resolve_account(plamenu_user.acct)
        assert account, "Pleroma could not resolve the Plamenu account"
        log(f"resolved to Pleroma account id {account['id']}")

    with step("follow: signed Follow -> plamenu inbox -> signed Accept -> Pleroma"):
        pleroma_bob.follow(account["id"])
        wait_for(
            lambda: pleroma_bob.relationship(account["id"])["following"],
            desc="Pleroma relationship to become following=true (our Accept processed)",
        )
        assert db.follower_count(plamenu_user.username) == 1, (
            "expected exactly one follow row in plamenu's db"
        )

    with step("bob appears in Plamenu's followers listing (client API)"):
        plamenu = Api(config.PLAMENU_URL)
        local = plamenu.lookup(plamenu_user.username)
        followers = plamenu.followers(local["id"])
        assert [f["acct"] for f in followers] == [f"bob@{config.PLEROMA_DOMAIN}"], (
            f"client-API followers should be exactly [bob@pleroma], got {followers!r}"
        )

    with step("unfollow: Undo(Follow) -> plamenu inbox"):
        pleroma_bob.unfollow(account["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the follow row to disappear from plamenu's db after Undo",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_follows_and_unfollows_plamenu_user"
)
def test_plamenu_follows_and_unfollows_pleroma_user(
    pleroma_bob, plamenu_user, plamenu_api, cli, db
):
    """Outbound: a Plamenu user follows the Pleroma user. Plamenu sends a signed
    Follow; Pleroma auto-accepts and returns a signed Accept, which Plamenu must
    verify to clear the follow's `pending` flag. Pleroma then lists the Plamenu
    user as a follower. The symmetric other half: Undo(Follow) (via the client
    API — there is no CLI unfollow verb) removes the Plamenu user from Pleroma's
    follower list, mirroring the inbound test above."""
    bob_acct = f"bob@{config.PLEROMA_DOMAIN}"

    with step(f"{plamenu_user.username} follows {bob_acct} via the Plamenu CLI"):
        cli.follow(plamenu_user.username, bob_acct)

    with step("Pleroma auto-Accepts: the outbound follow clears `pending`"):
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="plamenu's outbound follow to become accepted (pending=false)",
        )

    with step(f"{plamenu_user.acct} shows up as a follower on Pleroma"):
        bob = pleroma_bob.get("/api/v1/accounts/verify_credentials")
        wait_for(
            lambda: any(
                f["acct"] == plamenu_user.acct for f in pleroma_bob.followers(bob["id"])
            ),
            desc=f"{plamenu_user.acct} to appear in bob's Pleroma followers",
        )

    with step("the Plamenu user unfollows bob (Undo(Follow) via the client API)"):
        bob_local = plamenu_api.lookup(bob_acct)
        assert bob_local, "Plamenu must already know bob from the follow"
        plamenu_api.unfollow(bob_local["id"])
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is None,
            desc="the outbound follow row to be gone after the Undo",
        )

    with step(f"{plamenu_user.acct} disappears from bob's Pleroma followers"):
        wait_for(
            lambda: (
                not any(
                    f["acct"] == plamenu_user.acct
                    for f in pleroma_bob.followers(bob["id"])
                )
            ),
            desc=f"{plamenu_user.acct} to be removed from bob's Pleroma followers",
        )
