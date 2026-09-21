"""Baseline bidirectional federation between Plamenu and Akkoma (pleroma.local).

The reaction/follow/rich-text interop lives in test_pleroma_reactions.py,
test_pleroma_follow.py and test_rich_text.py; this module proves the ordinary
*microblogging* surface both ways — Create/Update/Delete, favourite/boost with
Undo, poll ingestion + voting, replies + mentions, profile Update, block, and
image-attachment metadata — the same contract the Mastodon and GoToSocial
suites hold. The peer runs Akkoma (a Pleroma fork) and speaks the Mastodon
client API, so the tests reuse `Api` through the `pleroma_bob` fixture; it is
optional and skips when the instance is down.
"""

import pytest

from plamenu_e2e import config, pleroma, unique
from plamenu_e2e.media import cached_attachment, make_png, tiny_png
from plamenu_e2e.steps import step, wait_for

BOB = f"{pleroma.BOB_NICK}@{config.PLEROMA_DOMAIN}"


def _plamenu_follows_bob(cli, plamenu_user, db) -> None:
    """Have the local user follow bob (auto-accepted) so bob's activities are
    delivered to Plamenu's inbox."""
    cli.follow(plamenu_user.username, BOB)
    wait_for(
        lambda: db.outbound_follow_pending(plamenu_user.username) is False,
        desc="the outbound follow to bob to be accepted (pending=false)",
    )


def _bob_follows_plamenu(pleroma_bob, plamenu_user) -> dict:
    """Have bob follow the local user (Plamenu accounts are unlocked, so the
    Accept is automatic); the resolved Akkoma account entity for the user."""
    account = pleroma_bob.resolve_account(plamenu_user.acct)
    assert account, f"Akkoma could not resolve {plamenu_user.acct}"
    pleroma_bob.follow(account["id"])
    wait_for(
        lambda: pleroma_bob.relationship(account["id"])["following"],
        desc="bob's follow of the plamenu user to be accepted",
    )
    return account


# ── posts: create / edit / delete ─────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_post_edit_delete_reach_pleroma"
)
def test_pleroma_post_edit_delete_reach_plamenu(
    pleroma_bob, plamenu_user, plamenu_api, cli, db, marker
):
    """Akkoma -> Plamenu: Create reaches the follower's home timeline, Update
    rewrites it, Delete tombstones it."""
    with step("a plamenu user follows bob"):
        _plamenu_follows_bob(cli, plamenu_user, db)

    with step("bob posts; the status reaches the follower's home timeline"):
        posted = pleroma_bob.post_status(f"hello from akkoma {marker}")
        got = wait_for(
            lambda: plamenu_api.home_status_containing(marker),
            desc="bob's post to reach the plamenu home timeline",
        )

    with step("bob edits; the Update rewrites the ingested copy"):
        edited = unique("plredit")
        pleroma_bob.edit_status(posted["id"], f"edited on akkoma {edited}")
        wait_for(
            lambda: (
                edited
                in (plamenu_api.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="bob's edit to reach plamenu",
        )
        assert db.status_edited_at(int(got["id"])) is not None

    with step("bob deletes; the copy is tombstoned"):
        pleroma_bob.delete_status(posted["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(got["id"]) is None,
            desc="bob's delete to reach plamenu",
        )
        wait_for(
            lambda: db.status_id_containing(marker) is None,
            desc="the stored row to disappear",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_post_edit_delete_reach_plamenu"
)
def test_plamenu_post_edit_delete_reach_pleroma(
    pleroma_bob, plamenu_user, plamenu_api, db, marker
):
    """Plamenu -> Akkoma: same lifecycle, opposite direction. Unlike GtS 0.22,
    Akkoma has no edit-then-delete defect, so the delete is asserted directly."""
    with step("bob follows the plamenu user"):
        _bob_follows_plamenu(pleroma_bob, plamenu_user)

    with step("plamenu posts; the Create is ingested by Akkoma"):
        posted = plamenu_api.post_status(f"hello from plamenu {marker}")
        got = wait_for(
            lambda: pleroma_bob.resolve_status(posted["uri"]),
            desc="the plamenu post to be ingested by Akkoma",
        )

    with step("plamenu edits; Akkoma applies the Update"):
        edited = unique("plamedit")
        plamenu_api.edit_status(posted["id"], f"edited on plamenu {edited}")
        wait_for(
            lambda: (
                edited
                in (pleroma_bob.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="the edit to reach Akkoma",
        )

    with step("plamenu deletes; the Delete drains and Akkoma drops its copy"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: db.pending_deliveries_to(config.PLEROMA_DOMAIN) == 0,
            desc="the Delete delivery to Akkoma to drain",
        )
        wait_for(
            lambda: pleroma_bob.get_status_or_none(got["id"]) is None,
            desc="the deleted status to disappear from Akkoma",
        )


# ── favourites & boosts ───────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_favourite_and_boost_plamenu_to_pleroma"
)
def test_favourite_and_boost_pleroma_to_plamenu(pleroma_bob, plamenu_api, db, marker):
    """Bob favourites and boosts a Plamenu post; Like/Announce arrive as counts
    and notifications, and Undo rolls them back."""
    with step("plamenu posts; bob resolves it"):
        local = plamenu_api.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: pleroma_bob.resolve_status(local["uri"]),
            desc="bob to resolve the plamenu post",
        )

    with step("bob favourites and boosts"):
        pleroma_bob.favourite(got["id"])
        pleroma_bob.reblog(got["id"])

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
            lambda: plamenu_api.notifications_from(BOB, "favourite"),
            desc="a favourite notification from bob",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(BOB, "reblog"),
            desc="a reblog notification from bob",
        )
        assert [a["acct"] for a in plamenu_api.favourited_by(local["id"])] == [BOB]
        assert [a["acct"] for a in plamenu_api.reblogged_by(local["id"])] == [BOB]
        assert db.favourite_count(int(local["id"])) == 1
        assert db.reblog_count(int(local["id"])) == 1

    with step("bob undoes both; counts roll back to zero"):
        pleroma_bob.unfavourite(got["id"])
        pleroma_bob.unreblog(got["id"])
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["favourites_count"] == 0,
            desc="the favourite count to roll back",
        )
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] == 0,
            desc="the reblog count to roll back",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_favourite_and_boost_pleroma_to_plamenu"
)
def test_favourite_and_boost_plamenu_to_pleroma(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """The opposite direction: Plamenu favourites and boosts an Akkoma post."""
    with step("bob posts; plamenu resolves it"):
        pleroma_status = pleroma_bob.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(pleroma_status["uri"]),
            desc="plamenu to resolve bob's post",
        )

    with step("plamenu favourites and boosts; local flags flip"):
        fav = plamenu_api.favourite(got["id"])
        assert fav["favourited"] is True
        boost = plamenu_api.reblog(got["id"])
        assert boost["reblog"]["reblogged"] is True

    with step("both arrive on Akkoma: counts + notifications"):
        wait_for(
            lambda: (
                pleroma_bob.get_status(pleroma_status["id"])["favourites_count"] >= 1
            ),
            desc="Akkoma favourites_count to rise",
        )
        wait_for(
            lambda: pleroma_bob.get_status(pleroma_status["id"])["reblogs_count"] >= 1,
            desc="Akkoma reblogs_count to rise",
        )
        wait_for(
            lambda: pleroma_bob.notifications_from(plamenu_user.acct, "favourite"),
            desc="favourite notification on Akkoma",
        )
        wait_for(
            lambda: pleroma_bob.notifications_from(plamenu_user.acct, "reblog"),
            desc="reblog notification on Akkoma",
        )

    with step("plamenu undoes both; Akkoma counts roll back"):
        plamenu_api.unfavourite(got["id"])
        plamenu_api.unreblog(got["id"])
        wait_for(
            lambda: (
                pleroma_bob.get_status(pleroma_status["id"])["favourites_count"] == 0
            ),
            desc="Akkoma favourites_count to roll back",
        )
        wait_for(
            lambda: pleroma_bob.get_status(pleroma_status["id"])["reblogs_count"] == 0,
            desc="Akkoma reblogs_count to roll back",
        )


# ── polls ─────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_votes_on_pleroma_poll"
)
def test_pleroma_votes_on_plamenu_poll(pleroma_bob, plamenu_api, db, marker):
    """A Plamenu-originated Question is rendered as a poll on Akkoma, bob's
    remote vote tallies on Plamenu, and the Update(Question) refreshes Akkoma."""
    with step("plamenu posts a poll; bob resolves it as a poll"):
        posted = plamenu_api.post_poll(f"poll {marker}", ["tea", "coffee"])
        got = wait_for(
            lambda: pleroma_bob.resolve_status(posted["uri"]),
            desc="bob to resolve the plamenu poll",
        )
        assert got["poll"], f"Akkoma ingested a poll-less status: {got!r}"

    with step("bob votes; the tally updates on plamenu"):
        pleroma_bob.vote(got["poll"]["id"], [0])
        wait_for(
            lambda: db.poll_tallies(int(posted["poll"]["id"])) == [1, 0],
            desc="bob's federated vote to tally on the plamenu poll",
        )
        assert plamenu_api.get_poll(posted["poll"]["id"])["votes_count"] == 1

    with step("the tally Update(Question) reaches Akkoma"):
        wait_for(
            lambda: pleroma_bob.get_poll(got["poll"]["id"])["votes_count"] == 1,
            desc="Akkoma's copy of the poll to show the refreshed tally",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_votes_on_plamenu_poll"
)
def test_plamenu_votes_on_pleroma_poll(pleroma_bob, plamenu_api, db, marker):
    """The opposite direction: Akkoma originates a poll, Plamenu ingests it and
    its federated vote tallies on Akkoma's origin poll."""
    with step("bob posts a poll; plamenu ingests it"):
        posted = pleroma_bob.post_poll(f"poll {marker}", ["red", "blue"])
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve bob's poll",
        )
        assert got["poll"], f"plamenu ingested a poll-less status: {got!r}"
        stored = wait_for(
            lambda: db.poll_for_status(int(got["id"])),
            desc="the poll row for the inbound Question",
        )
        _, options, _ = stored
        assert options == ["red", "blue"]

    with step("the plamenu user votes; the origin poll tallies it"):
        voted = plamenu_api.vote(got["poll"]["id"], [1])
        assert voted["own_votes"] == [1]
        assert voted["voted"] is True
        wait_for(
            lambda: pleroma_bob.get_poll(posted["poll"]["id"])["votes_count"] == 1,
            desc="the federated vote to tally on Akkoma's origin poll",
        )
        origin = pleroma_bob.get_poll(posted["poll"]["id"])
        assert origin["options"][1]["votes_count"] == 1


# ── replies & mentions ────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_reply_and_mention_plamenu_to_pleroma"
)
def test_reply_and_mention_pleroma_to_plamenu(
    pleroma_bob, plamenu_user, plamenu_api, db, marker
):
    """Akkoma -> Plamenu: a reply without an @handle still carries a wire
    Mention tag and notifies the parent author.

    The final Pleroma-family ActivityPub serializer derives Mention tags from
    known actors in `to`, independently of the visible reply text.
    """
    with step("plamenu posts; bob resolves it"):
        local = plamenu_api.post_status(f"reply to me from akkoma {marker}")
        got = wait_for(
            lambda: pleroma_bob.resolve_status(local["uri"]),
            desc="bob to resolve the plamenu post",
        )

    with step("bob replies without typing a mention"):
        pleroma_bob.post_status(
            f"a tagless reply from akkoma {marker}r",
            in_reply_to_id=got["id"],
        )

    with step("the reply arrives threaded under the plamenu post"):
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the akkoma reply to arrive in plamenu statuses",
        )
        assert db.status_parent_id(reply_id) == int(local["id"])

    with step("the author gets a mention notification and context reflects it"):
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(BOB, "mention"),
            desc="a mention notification from bob",
        )
        assert notifs[0]["status"]["id"] == str(reply_id)
        assert plamenu_user.username in [
            m["acct"] for m in notifs[0]["status"]["mentions"]
        ], "Akkoma's serializer should emit the parent as a Mention tag"
        assert plamenu_api.get_status(local["id"])["replies_count"] >= 1
        assert any(
            f"{marker}r" in s["content"]
            for s in plamenu_api.context(local["id"])["descendants"]
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_reply_and_mention_pleroma_to_plamenu"
)
def test_reply_and_mention_plamenu_to_pleroma(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """The opposite direction: a Plamenu reply mentioning bob threads under his
    post and notifies him."""
    with step("bob posts; plamenu resolves it"):
        pleroma_status = pleroma_bob.post_status(f"reply to me from plamenu {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(pleroma_status["uri"]),
            desc="plamenu to resolve bob's post",
        )

    with step("plamenu replies, mentioning bob"):
        reply = plamenu_api.post_status(
            f"@{BOB} a reply from plamenu {marker}r", in_reply_to_id=got["id"]
        )
        assert reply["in_reply_to_id"] == got["id"]
        assert BOB in [m["acct"] for m in reply["mentions"]]

    with step("bob gets a mention notification threaded onto his post"):
        notifs = wait_for(
            lambda: pleroma_bob.notifications_from(plamenu_user.acct, "mention"),
            desc="a mention notification from the plamenu user on Akkoma",
        )
        assert notifs[0]["status"]["in_reply_to_id"] == pleroma_status["id"]

    with step("his replies_count and context reflect the reply"):
        wait_for(
            lambda: pleroma_bob.get_status(pleroma_status["id"])["replies_count"] >= 1,
            desc="bob's replies_count to reach 1",
        )
        assert any(
            f"{marker}r" in s["content"]
            for s in pleroma_bob.context(pleroma_status["id"])["descendants"]
        )


# ── profile updates ───────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_profile_update_plamenu_to_pleroma"
)
def test_profile_update_pleroma_to_plamenu(pleroma_bob, plamenu_user, cli, db, marker):
    """Akkoma -> Plamenu: bob's profile Update refreshes the stored remote
    account's display name and avatar."""
    with step("the plamenu user follows bob"):
        _plamenu_follows_bob(cli, plamenu_user, db)

    with step("bob edits his profile on Akkoma"):
        new_name = f"Bob {marker}"
        pleroma_bob.update_profile(
            files={"avatar": ("avatar.png", tiny_png((30, 30, 200)), "image/png")},
            display_name=new_name,
        )

    with step("Plamenu applies the Update(Actor)"):
        wait_for(
            lambda: (
                db.remote_display_name(pleroma.BOB_NICK, config.PLEROMA_DOMAIN)
                == new_name
            ),
            desc="the stored remote display name to update",
        )
        avatar = db.remote_avatar_url(pleroma.BOB_NICK, config.PLEROMA_DOMAIN)
        assert avatar and avatar.startswith("https://"), avatar


@pytest.mark.federation(
    direction="outbound", reverse_of="test_profile_update_pleroma_to_plamenu"
)
def test_profile_update_plamenu_to_pleroma(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """The opposite direction: a Plamenu profile edit refreshes bob's cached
    copy of the account (display name + note + avatar)."""
    with step("bob follows the plamenu user so Updates reach his server"):
        _bob_follows_plamenu(pleroma_bob, plamenu_user)

    with step("edit the profile through the Plamenu client API"):
        new_name = f"Renamed {marker}"
        entity = plamenu_api.update_profile(
            files={"avatar": ("avatar.png", tiny_png(), "image/png")},
            display_name=new_name,
            note=f"bio {marker}",
        )
        assert entity["display_name"] == new_name

    with step("Akkoma applies the Update(Actor)"):
        wait_for(
            lambda: (
                (pleroma_bob.lookup(plamenu_user.acct) or {}).get("display_name")
                == new_name
            ),
            desc="bob's cached copy to update its display name",
        )
        refreshed = pleroma_bob.lookup(plamenu_user.acct)
        assert marker in refreshed["note"]
        assert "missing" not in (refreshed["avatar"] or "")


# ── blocks ────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_block_reaches_plamenu"
)
def test_plamenu_block_severs_pleroma_follow(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """Outbound Block: bob follows the user; the user's block severs the follow
    on the Akkoma side."""
    with step("bob follows the plamenu user"):
        account = _bob_follows_plamenu(pleroma_bob, plamenu_user)

    with step("the plamenu user blocks bob; Akkoma severs the follow"):
        bob_on_plamenu = plamenu_api.resolve_account(BOB)
        rel = plamenu_api.block(bob_on_plamenu["id"])
        assert rel["blocking"] is True and rel["followed_by"] is False
        assert [b["acct"] for b in plamenu_api.blocks()] == [BOB]
        wait_for(
            lambda: not pleroma_bob.relationship(account["id"])["following"],
            desc="bob's follow to be severed by the Block",
        )

    with step("unblock lifts it locally"):
        plamenu_api.unblock(bob_on_plamenu["id"])
        assert plamenu_api.blocks() == []


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_block_severs_pleroma_follow"
)
def test_pleroma_block_reaches_plamenu(
    pleroma_bob, plamenu_user, plamenu_api, cli, db, marker
):
    """Inbound Block from Akkoma — DOCUMENTED UPSTREAM GAP: Akkoma ships
    `outgoing_blocks: false` by default (and the test instance does not
    override it), so its Block/Undo(Block) are never federated and Plamenu's
    inbox never sees them. Plamenu's inbound handling is proven against Mastodon
    (test_blocks.py::test_block_federates_from_mastodon); this is a peer config
    limitation, not a Plamenu bug, so the test is skipped rather than left to
    time out."""
    pytest.skip(
        "Akkoma outgoing_blocks=false by default; Block/Undo(Block) are not "
        "federated (pleroma-test does not enable them). Inbound handling is "
        "covered by test_blocks.py::test_block_federates_from_mastodon."
    )


# ── media ─────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_image_metadata_plamenu_to_pleroma"
)
def test_image_metadata_pleroma_to_plamenu(
    pleroma_bob, plamenu_user, plamenu_api, cli, db, marker
):
    """Akkoma -> Plamenu: alt text lands at ingest, then Plamenu caches the
    file and computes its own blurhash + dimensions (Akkoma federates neither
    blurhash nor focal point nor dimensions)."""
    with step("the plamenu user follows bob"):
        _plamenu_follows_bob(cli, plamenu_user, db)

    with step("bob posts an image with alt text"):
        up = pleroma_bob.upload_media(
            make_png(),
            filename="akkoma.png",
            mime="image/png",
            description="an akkoma picture",
        )
        pleroma_bob.post_with_media(f"akkoma media {marker}", [up["id"]])

    with step("the stored plamenu row carries the alt text + content_type"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="bob's media post to arrive in plamenu statuses",
        )
        rows = wait_for(
            lambda: db.media_for_status(status_id) or None, desc="the attachment row"
        )
        content_type, description, *_ = rows[0]
        assert content_type.startswith("image/"), content_type
        assert description == "an akkoma picture", description

    with step("Plamenu caches the file and computes its own blurhash + dimensions"):
        att = wait_for(
            lambda: cached_attachment(plamenu_api, status_id, require_blurhash=True),
            desc="Plamenu to proxy/cache the image onto its own /media/ route",
        )
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/"), att["url"]
        assert att["blurhash"], att
        assert att["meta"]["original"]["width"] and att["meta"]["original"]["height"], (
            att["meta"]
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_image_metadata_pleroma_to_plamenu"
)
def test_image_metadata_plamenu_to_pleroma(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """Plamenu -> Akkoma: the alt text and image type survive; the focal point
    is intentionally not asserted (Akkoma's attachment schema has no focal-point
    field, so it is dropped)."""
    with step("bob follows the plamenu user"):
        _bob_follows_plamenu(pleroma_bob, plamenu_user)

    with step("upload an image with alt + focus on Plamenu"):
        uploaded = plamenu_api.upload_media(
            make_png(),
            filename="pic.png",
            mime="image/png",
            description="a plamenu picture",
            focus="-0.5,0.3",
        )
        assert uploaded["blurhash"]

    with step("post it; bob resolves the copy by uri"):
        local = plamenu_api.post_with_media(
            f"plamenu picture {marker}", [uploaded["id"]]
        )
        got = wait_for(
            lambda: pleroma_bob.resolve_status(local["uri"]),
            desc="bob to resolve the plamenu media post",
        )

    with step("bob's copy carries the alt text + image type (focus dropped)"):
        att = got["media_attachments"][0]
        assert att["type"] == "image", att
        assert att["description"] == "a plamenu picture", att
