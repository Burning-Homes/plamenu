"""Follow federation between Plamenu and real upstream Pleroma (plup.local).

This is the regression guard for the RFC 9421 black-hole bug
(plamenu/RFC9421_PLEROMA_FIX_PLAN.md). Upstream Pleroma 2.10 does NOT implement
RFC 9421: it returns HTTP 200 on an inbox POST and then asynchronously drops
any activity whose signature it can't parse. Plamenu used to infer RFC 9421
support from that 200 and permanently black-hole every subsequent delivery to
the peer, so an outbound follow hung "pending" forever. The fix defaults to
draft-cavage and only emits RFC 9421 to a peer proven (by its own inbound RFC
9421 traffic) to speak it — so these follows land, and no false-positive
`host_signature_prefs.rfc9421=true` row is ever learned for plup.local.

The Akkoma peer (pleroma.local, test_pleroma_follow.py) tolerates our
signatures and never reproduced this; that is why plup is a separate peer.
"""

import pytest
from plamenu_e2e import config, plup
from plamenu_e2e.api import Api
from plamenu_e2e.steps import log, step, wait_for

MARI = f"{plup.MARI_NICK}@{config.PLUP_DOMAIN}"


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_follows_and_unfollows_plup_user"
)
def test_plup_follows_and_unfollows_plamenu_user(plup_mari, plamenu_user, db):
    """Inbound: upstream Pleroma discovers a Plamenu user and follows it (signed
    Follow into our inbox -> signed Accept back), then Undo(Follow) removes the
    row. Pleroma signs with draft-cavage, which we verify as before."""
    with step(f"resolve @{plamenu_user.acct} from upstream Pleroma"):
        account = plup_mari.resolve_account(plamenu_user.acct)
        assert account, "upstream Pleroma could not resolve the Plamenu account"
        log(f"resolved to Pleroma account id {account['id']}")

    with step("follow: signed Follow -> plamenu inbox -> signed Accept -> Pleroma"):
        plup_mari.follow(account["id"])
        wait_for(
            lambda: plup_mari.relationship(account["id"])["following"],
            desc="Pleroma relationship to become following=true (our Accept processed)",
        )
        assert db.follower_count(plamenu_user.username) == 1, (
            "expected exactly one follow row in plamenu's db"
        )

    with step("mari appears in Plamenu's followers listing (client API)"):
        plamenu = Api(config.PLAMENU_URL)
        local = plamenu.lookup(plamenu_user.username)
        followers = plamenu.followers(local["id"])
        assert [f["acct"] for f in followers] == [MARI], (
            f"client-API followers should be exactly [{MARI}], got {followers!r}"
        )

    with step("unfollow: Undo(Follow) -> plamenu inbox"):
        plup_mari.unfollow(account["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the follow row to disappear from plamenu's db after Undo",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_plup_follows_and_unfollows_plamenu_user"
)
def test_plamenu_follows_and_unfollows_plup_user(
    plup_mari, plamenu_user, plamenu_api, cli, db
):
    """Outbound: a Plamenu user follows the upstream-Pleroma user. THE regression
    guard: pre-fix Plamenu double-knocked RFC 9421, Pleroma 200'd and silently
    dropped the Follow, and this hung on `pending` forever. Post-fix Plamenu
    sends draft-cavage, Pleroma auto-accepts and returns a verifiable Accept,
    and `pending` clears.

    This runs under default settings (`emit_rfc9421` on) so the guard is real,
    and asserts afterwards that NO `host_signature_prefs.rfc9421=true` row was
    learned for plup.local — the exact false-positive the old 200 heuristic
    minted — yet delivery still succeeded."""
    with step(f"{plamenu_user.username} follows {MARI} via the Plamenu CLI"):
        cli.follow(plamenu_user.username, MARI)

    with step("Pleroma auto-Accepts: the outbound follow clears `pending`"):
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="plamenu's outbound follow to become accepted (pending=false)",
        )

    with step(f"{plamenu_user.acct} shows up as a follower on upstream Pleroma"):
        mari = plup_mari.get("/api/v1/accounts/verify_credentials")
        wait_for(
            lambda: any(
                f["acct"] == plamenu_user.acct for f in plup_mari.followers(mari["id"])
            ),
            desc=f"{plamenu_user.acct} to appear in mari's Pleroma followers",
        )

    with step("regression: no false-positive rfc9421=true row learned for plup.local"):
        # Upstream Pleroma never signs us RFC 9421, so the only way a positive
        # row could exist is the old (removed) 200 heuristic. A cavage delivery
        # records nothing, so the row must be absent — delivery succeeded above
        # without it, which is exactly the fix.
        assert db.rfc9421_pref(config.PLUP_DOMAIN) is None, (
            "plup.local must not have earned an rfc9421 preference row "
            f"(got {db.rfc9421_pref(config.PLUP_DOMAIN)!r})"
        )

    with step("the Plamenu user unfollows mari (Undo(Follow) via the client API)"):
        mari_local = plamenu_api.lookup(MARI)
        assert mari_local, "Plamenu must already know mari from the follow"
        plamenu_api.unfollow(mari_local["id"])
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is None,
            desc="the outbound follow row to be gone after the Undo",
        )

    with step(f"{plamenu_user.acct} disappears from mari's Pleroma followers"):
        wait_for(
            lambda: (
                not any(
                    f["acct"] == plamenu_user.acct
                    for f in plup_mari.followers(mari["id"])
                )
            ),
            desc=f"{plamenu_user.acct} to be removed from mari's Pleroma followers",
        )
