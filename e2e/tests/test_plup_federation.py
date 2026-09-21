"""Baseline bidirectional federation between Plamenu and upstream Pleroma.

The follow handshake and reactions live in test_plup_follow.py and
test_plup_reactions.py; this module proves the ordinary microblogging surface
both ways against real upstream Pleroma 2.10.2 (plup.local) — Create/Update/
Delete and favourite/boost with Undo. Every one of these rides the same signed
delivery path that the RFC 9421 black-hole bug broke, so they double as
regression coverage for that fix (plamenu/RFC9421_PLEROMA_FIX_PLAN.md).

Distinct from test_pleroma_federation.py, which runs against the Akkoma fork
(pleroma.local); upstream Pleroma is a separate peer with its own signature
behavior. Optional: skips when plup isn't running.
"""

import pytest
from plamenu_e2e import config, plup, unique
from plamenu_e2e.steps import step, wait_for

MARI = f"{plup.MARI_NICK}@{config.PLUP_DOMAIN}"


def _plamenu_follows_mari(plup_mari, cli, plamenu_user, db) -> None:
    """Have the local user follow mari (auto-accepted) so mari's activities are
    delivered to Plamenu's inbox. Check both sides of the relationship: a
    remote Accept alone is not proof that Pleroma retained the follower."""
    cli.follow(plamenu_user.username, MARI)
    wait_for(
        lambda: db.outbound_follow_pending(plamenu_user.username) is False,
        desc="the outbound follow to mari to be accepted (pending=false)",
    )
    mari = plup_mari.get("/api/v1/accounts/verify_credentials")
    wait_for(
        lambda: any(
            follower["acct"] == plamenu_user.acct
            for follower in plup_mari.followers(mari["id"])
        ),
        desc="the plamenu user to be registered in mari's Pleroma followers",
    )


def _mari_follows_plamenu(plup_mari, plamenu_user) -> dict:
    """Have mari follow the local user (Plamenu accounts are unlocked, so the
    Accept is automatic); returns the resolved Pleroma account entity."""
    account = plup_mari.resolve_account(plamenu_user.acct)
    assert account, f"upstream Pleroma could not resolve {plamenu_user.acct}"
    plup_mari.follow(account["id"])
    wait_for(
        lambda: plup_mari.relationship(account["id"])["following"],
        desc="mari's follow of the plamenu user to be accepted",
    )
    return account


# ── posts: create / edit / delete ─────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_post_edit_delete_reach_plup"
)
def test_plup_post_edit_delete_reach_plamenu(
    plup_mari, plamenu_user, plamenu_api, cli, db, marker
):
    """upstream Pleroma -> Plamenu: Create reaches the follower's home timeline,
    Update rewrites it, Delete tombstones it."""
    with step("a plamenu user follows mari"):
        _plamenu_follows_mari(plup_mari, cli, plamenu_user, db)

    with step("mari posts; the status reaches the follower's home timeline"):
        posted = plup_mari.post_status(f"hello from upstream pleroma {marker}")
        got = wait_for(
            lambda: plamenu_api.home_status_containing(marker),
            desc="mari's post to reach the plamenu home timeline",
        )

    with step("mari edits; the Update rewrites the ingested copy"):
        edited = unique("pluedit")
        plup_mari.edit_status(posted["id"], f"edited on upstream pleroma {edited}")
        wait_for(
            lambda: (
                edited
                in (plamenu_api.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="mari's edit to reach plamenu",
        )
        assert db.status_edited_at(int(got["id"])) is not None

    with step("mari deletes; the copy is tombstoned"):
        plup_mari.delete_status(posted["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(got["id"]) is None,
            desc="mari's delete to reach plamenu",
        )
        wait_for(
            lambda: db.status_id_containing(marker) is None,
            desc="the stored row to disappear",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason=(
        "Pleroma's signed authorized-fetch request is intrinsically peer-to-Plamenu; "
        "the reverse implementation does not make the same path-only signature."
    ),
)
def test_plup_path_only_signed_collection_pages_are_authorized(
    plup_mari, plamenu_user, plamenu_api
):
    """Pleroma signs paginated GETs over the URL path but omits the query.

    Pleroma's external-user sync fetches the actor's followers/following
    envelopes and then each ``?page=1`` link to detect whether the collection
    is private. Plamenu uses authorized fetch in the E2E configuration.
    Rejecting Pleroma's path-only signature therefore makes both public
    collections look private.
    """
    with step(f"resolve fresh @{plamenu_user.acct} from upstream Pleroma"):
        account = plup_mari.resolve_account(plamenu_user.acct)
        assert account, f"upstream Pleroma could not resolve {plamenu_user.acct}"
        assert account["acct"] == plamenu_user.acct, (
            f"Pleroma resolved the wrong account: {account['acct']!r}"
        )

    with step("refresh Pleroma's stored follow metadata via signed AP fetches"):
        actor_uri = plamenu_api.ap_get(f"/users/{plamenu_user.username}")["id"]
        plup.refresh_follow_information(actor_uri)
        account = plup_mari.account(account["id"])
        assert account, "the freshly resolved Plamenu account disappeared from Pleroma"

    with step("Pleroma recognized both signed collection pages as public"):
        assert account["pleroma"]["hide_follows"] is False, (
            "Pleroma treated Plamenu's following collection as private; its "
            "path-only signed GET of ?page=1 was probably rejected"
        )
        assert account["pleroma"]["hide_followers"] is False, (
            "Pleroma treated Plamenu's followers collection as private; its "
            "path-only signed GET of ?page=1 was probably rejected"
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_plup_post_edit_delete_reach_plamenu"
)
def test_plamenu_post_edit_delete_reach_plup(
    plup_mari, plamenu_user, plamenu_api, db, marker
):
    """Plamenu -> upstream Pleroma: same lifecycle, opposite direction. Every
    step here is a signed delivery to plup — the exact path that used to be
    black-holed."""
    with step("mari follows the plamenu user"):
        _mari_follows_plamenu(plup_mari, plamenu_user)

    with step("plamenu posts; the Create is ingested by upstream Pleroma"):
        posted = plamenu_api.post_status(f"hello from plamenu {marker}")
        got = wait_for(
            lambda: plup_mari.resolve_status(posted["uri"]),
            desc="the plamenu post to be ingested by upstream Pleroma",
        )

    with step("plamenu edits; upstream Pleroma applies the Update"):
        edited = unique("plamedit")
        plamenu_api.edit_status(posted["id"], f"edited on plamenu {edited}")
        wait_for(
            lambda: (
                edited
                in (plup_mari.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="the edit to reach upstream Pleroma",
        )

    with step("plamenu deletes; the Delete drains and upstream Pleroma drops its copy"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: db.pending_deliveries_to(config.PLUP_DOMAIN) == 0,
            desc="the Delete delivery to upstream Pleroma to drain",
        )
        wait_for(
            lambda: plup_mari.get_status_or_none(got["id"]) is None,
            desc="the deleted status to disappear from upstream Pleroma",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="Upstream Pleroma's reply serializer is the producer behavior under test; Plamenu's reverse reply path is already covered against the Pleroma-family peer.",
)
def test_plup_reply_without_typed_mention_reaches_plamenu(
    plup_mari, plamenu_user, plamenu_api, db, marker
):
    """Upstream Pleroma adds a real Mention tag for a reply parent found in
    `to`, even when the author typed no @handle. Plamenu therefore needs no
    notification fallback based on bare audience addressing."""
    with step("mari resolves a fresh Plamenu post"):
        local = plamenu_api.post_status(f"reply to me from plup {marker}")
        remote = wait_for(
            lambda: plup_mari.resolve_status(local["uri"]),
            desc="upstream Pleroma to resolve the Plamenu post",
        )

    with step("mari replies without typing the Plamenu author's handle"):
        plup_mari.post_status(
            f"reply from upstream Pleroma {marker}r",
            in_reply_to_id=remote["id"],
        )
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the upstream Pleroma reply to reach Plamenu",
        )
        assert db.status_parent_id(reply_id) == int(local["id"])

    with step("Pleroma's generated Mention tag produces the notification"):
        notifications = wait_for(
            lambda: plamenu_api.notifications_from(MARI, "mention"),
            desc="the parent author to receive a mention notification",
        )
        assert notifications[0]["status"]["id"] == str(reply_id)
        assert plamenu_user.username in [
            mention["acct"] for mention in notifications[0]["status"]["mentions"]
        ]


# ── favourites & boosts ───────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_favourite_and_boost_plamenu_to_plup"
)
def test_favourite_and_boost_plup_to_plamenu(plup_mari, plamenu_api, db, marker):
    """mari favourites and boosts a Plamenu post; Like/Announce arrive as counts
    and notifications, and Undo rolls them back."""
    with step("plamenu posts; mari resolves it"):
        local = plamenu_api.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: plup_mari.resolve_status(local["uri"]),
            desc="mari to resolve the plamenu post",
        )

    with step("mari favourites and boosts"):
        plup_mari.favourite(got["id"])
        plup_mari.reblog(got["id"])

    with step("both arrive as counts + notifications on plamenu"):
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] >= 1,
            desc="the favourite count to rise",
        )
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] >= 1,
            desc="the reblog count to rise",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(MARI, "favourite"),
            desc="a favourite notification from mari",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(MARI, "reblog"),
            desc="a reblog notification from mari",
        )
        assert [a["acct"] for a in plamenu_api.favourited_by(local["id"])] == [MARI]
        assert [a["acct"] for a in plamenu_api.reblogged_by(local["id"])] == [MARI]
        assert db.favourite_count(int(local["id"])) == 1
        assert db.reblog_count(int(local["id"])) == 1

    with step("mari undoes both; counts roll back to zero"):
        plup_mari.unfavourite(got["id"])
        plup_mari.unreblog(got["id"])
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] == 0,
            desc="the favourite count to roll back",
        )
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] == 0,
            desc="the reblog count to roll back",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_favourite_and_boost_plup_to_plamenu"
)
def test_favourite_and_boost_plamenu_to_plup(
    plup_mari, plamenu_user, plamenu_api, marker
):
    """The opposite direction: Plamenu favourites and boosts an upstream-Pleroma
    post (both are signed deliveries to plup)."""
    with step("mari posts; plamenu resolves it"):
        plup_status = plup_mari.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(plup_status["uri"]),
            desc="plamenu to resolve mari's post",
        )

    with step("plamenu favourites and boosts; local flags flip"):
        fav = plamenu_api.favourite(got["id"])
        assert fav["favourited"] is True
        boost = plamenu_api.reblog(got["id"])
        assert boost["reblog"]["reblogged"] is True

    with step("both arrive on upstream Pleroma: counts + notifications"):
        wait_for(
            lambda: plup_mari.get_status(plup_status["id"])["favourites_count"] >= 1,
            desc="upstream Pleroma favourites_count to rise",
        )
        wait_for(
            lambda: plup_mari.get_status(plup_status["id"])["reblogs_count"] >= 1,
            desc="upstream Pleroma reblogs_count to rise",
        )
        wait_for(
            lambda: plup_mari.notifications_from(plamenu_user.acct, "favourite"),
            desc="favourite notification on upstream Pleroma",
        )
        wait_for(
            lambda: plup_mari.notifications_from(plamenu_user.acct, "reblog"),
            desc="reblog notification on upstream Pleroma",
        )

    with step("plamenu undoes both; upstream Pleroma counts roll back"):
        plamenu_api.unfavourite(got["id"])
        plamenu_api.unreblog(got["id"])
        wait_for(
            lambda: plup_mari.get_status(plup_status["id"])["favourites_count"] == 0,
            desc="upstream Pleroma favourites_count to roll back",
        )
        wait_for(
            lambda: plup_mari.get_status(plup_status["id"])["reblogs_count"] == 0,
            desc="upstream Pleroma reblogs_count to roll back",
        )
