"""Timing/ordering torture: rapid sequences of activities whose delivery
order federation does not guarantee. Each test fires a quick burst and then
asserts the *converged* state — and that it stays converged (a late,
out-of-order activity must not resurrect an older state)."""

import time

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


def assert_stays(check, *, desc: str, seconds: float = 8.0):
    """Assert `check()` holds now and keeps holding for `seconds` — the
    settle-guard against a late out-of-order activity flipping the state
    back after the first (correct) observation."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        assert check(), f"converged state regressed: {desc}"
        time.sleep(1)


def settled(read, *, desc: str, seconds: float = 15.0, timeout: float = 120.0):
    """The value `read()` returns once it has stopped changing for `seconds`.

    For a burst whose *origin* has not decided the answer either: sample the
    origin until it holds still, then hold this server to whatever it said."""
    deadline = time.monotonic() + timeout
    value, since = read(), time.monotonic()
    while time.monotonic() < deadline:
        time.sleep(1)
        current = read()
        if current != value:
            value, since = current, time.monotonic()
        elif time.monotonic() - since >= seconds:
            return value
    raise TimeoutError(f"{desc} never settled (last value: {value!r})")


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_rapid_edits_converge_on_mastodon"
)
def test_rapid_edits_converge_to_latest(alice, plamenu_user, cli, db, marker):
    """Covers: a burst of `Update(Note)`s — whatever order the deliveries
    land in, the stored status must converge on the *latest* revision and
    never flip back to an older one."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts and immediately edits twice"):
        masto_status = alice.post_status(f"revision one {marker}")
        alice.edit_status(masto_status["id"], f"revision two {marker}")
        alice.edit_status(masto_status["id"], f"revision three {marker}")

    with step("plamenu converges on revision three"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the (possibly already-edited) status to arrive",
        )

        def content():
            row = db.conn.execute(
                "SELECT content FROM statuses WHERE id = %s", (status_id,)
            ).fetchone()
            return row[0] if row else ""

        wait_for(
            lambda: "revision three" in content(),
            desc="the stored content to reach revision three",
        )
        log(f"converged content: {content()!r}")

    with step("and stays there — no older revision overwrites it back"):
        assert_stays(
            lambda: "revision three" in content(),
            desc="the status regressed to an older revision",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_boost_flipflop_converges_unboosted"
)
def test_boost_then_instant_unboost_converges(alice, plamenu_user, plamenu_api, marker):
    """Covers: outbound `Announce` chased immediately by `Undo(Announce)` —
    the burst is coalesced in Plamenu's delivery queue (the Announce sits out
    a short cancellation grace; the unboost cancels it in place and skips the
    `Undo`), so Mastodon settles on 'not boosted' deterministically."""
    with step("alice posts; plamenu resolves it"):
        masto_status = alice.post_status(f"flipflop boost {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(masto_status["uri"]),
            desc="Plamenu to resolve alice's status by URL",
        )

    with step("boost and instantly unboost"):
        plamenu_api.reblog(local["id"])
        plamenu_api.unreblog(local["id"])
        assert plamenu_api.get_status(local["id"])["reblogged"] is False

    with step("mastodon settles on zero boosts and stays there"):
        # Both deliveries must have been processed before the settle-guard
        # means anything; the queue draining shows as the count stabilizing.
        wait_for(
            lambda: (
                alice.get_status(masto_status["id"])["reblogs_count"] == 0
                and not alice.reblogged_by(masto_status["id"])
            ),
            desc="alice's copy to settle on zero boosts",
        )
        assert_stays(
            lambda: alice.get_status(masto_status["id"])["reblogs_count"] == 0,
            desc="a late Announce resurrected the boost",
        )


@pytest.mark.federation(
    direction="outbound",
    reverse_of="test_mastodon_favourite_flipflop_converges_with_the_origin",
)
def test_favourite_flipflop_settles_on_favourited(
    alice, plamenu_user, plamenu_api, marker
):
    """Covers: `Like`, `Undo(Like)`, `Like` again in one burst — the first
    `Like` is cancelled in-queue by the unfavourite, only the second one
    federates, so the origin server ends on exactly one favourite."""
    with step("alice posts; plamenu resolves it"):
        masto_status = alice.post_status(f"flipflop fav {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(masto_status["uri"]),
            desc="Plamenu to resolve alice's status by URL",
        )

    with step("favourite, unfavourite, favourite again"):
        plamenu_api.favourite(local["id"])
        plamenu_api.unfavourite(local["id"])
        entity = plamenu_api.favourite(local["id"])
        assert entity["favourited"] is True

    with step("mastodon settles on exactly one favourite and stays there"):
        wait_for(
            lambda: alice.get_status(masto_status["id"])["favourites_count"] == 1,
            desc="alice's favourites_count to settle at 1",
        )
        assert_stays(
            lambda: alice.get_status(masto_status["id"])["favourites_count"] == 1,
            desc="a late Like/Undo flipped the favourite state",
        )


# ── reverse ordering: Mastodon-originated bursts converge on Plamenu ─────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_boost_then_instant_unboost_converges"
)
def test_mastodon_boost_flipflop_converges_unboosted(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Reverse of test_boost_then_instant_unboost_converges: alice boosts a
    Plamenu post and instantly unboosts; Plamenu must settle on 'not boosted'
    and stay there (a late reordered Announce must not resurrect the boost)."""
    with step("a plamenu user posts; alice resolves it"):
        local = plamenu_api.post_status(f"masto flipflop boost {marker}")
        masto = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status",
        )

    with step("alice boosts and instantly unboosts"):
        alice.reblog(masto["id"])
        alice.unreblog(masto["id"])

    with step("Plamenu settles on zero boosts and stays there"):
        wait_for(
            lambda: (
                db.reblog_count(int(local["id"])) == 0
                and plamenu_api.reblogged_by(local["id"]) == []
            ),
            desc="Plamenu to settle on unboosted",
        )
        assert_stays(
            lambda: db.reblog_count(int(local["id"])) == 0,
            desc="a late Announce resurrected the boost",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_favourite_flipflop_settles_on_favourited"
)
def test_mastodon_favourite_flipflop_converges_with_the_origin(
    alice, plamenu_user, plamenu_api, db, marker
):
    """Reverse of test_favourite_flipflop_settles_on_favourited: alice
    favourites/unfavourites/favourites a Plamenu post, and this server must end
    on the answer *Mastodon* ended on.

    Not "on one favourite": unlike the outbound direction, this burst does not
    decide anything at the origin. Mastodon's unfavourite is a background job
    (`UnfavouriteWorker`) while its favourite is synchronous and returns early
    when a `Favourite` row still exists, so whenever the job lags the third call
    is a no-op — it answers `favourited: true` and then the job destroys the row
    underneath it, leaving the `Undo(Like)` as the last thing on the wire and
    Mastodon itself unfavourited. Both outcomes are routine on a busy instance,
    so the interoperability property is agreement, not a fixed count."""
    with step("a plamenu user posts; alice resolves it"):
        local = plamenu_api.post_status(f"masto flipflop fav {marker}")
        masto = wait_for(
            lambda: alice.resolve_status(local["uri"]),
            desc="Mastodon to resolve the Plamenu status",
        )

    with step("alice favourites, unfavourites, favourites again"):
        alice.favourite(masto["id"])
        alice.unfavourite(masto["id"])
        alice.favourite(masto["id"])

    with step("Plamenu ends where Mastodon ended, and stays there"):
        origin = settled(
            lambda: alice.get_status(masto["id"])["favourites_count"],
            desc="Mastodon's own favourites_count",
        )
        log(f"Mastodon settled on {origin} favourite(s)")
        wait_for(
            lambda: db.favourite_count(int(local["id"])) == origin,
            desc=f"Plamenu to agree on {origin} favourite(s)",
        )
        assert_stays(
            lambda: db.favourite_count(int(local["id"])) == origin,
            desc="a late Like/Undo flipped the favourite state",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_rapid_edits_converge_to_latest"
)
def test_plamenu_rapid_edits_converge_on_mastodon(
    alice, plamenu_user, plamenu_api, marker
):
    """Reverse of test_rapid_edits_converge_to_latest: a Plamenu burst of edits
    must converge on Mastodon at the newest revision and stay there (Mastodon's
    inbound edit LWW discards a stale Update)."""
    with step(f"alice follows @{plamenu_user.acct} so the edits reach her"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="alice follows before posting",
        )

    with step("the plamenu user posts and immediately edits twice"):
        status = plamenu_api.post_status(f"revision one {marker}")
        plamenu_api.edit_status(status["id"], f"revision two {marker}")
        plamenu_api.edit_status(status["id"], f"revision three {marker}")

    with step("Mastodon converges on revision three and stays there"):
        copy = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the post to reach alice",
        )
        wait_for(
            lambda: "revision three" in alice.get_status(copy["id"])["content"],
            desc="Mastodon to reach revision three",
        )
        assert_stays(
            lambda: "revision three" in alice.get_status(copy["id"])["content"],
            desc="an older Update regressed the content",
        )
