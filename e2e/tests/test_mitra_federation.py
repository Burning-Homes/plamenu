"""Baseline bidirectional federation between Plamenu and Mitra.

This intentionally tests only the shared microblogging surface. Mitra's paid
subscriptions and Monero features are explicit Plamenu non-goals; portable
actors, groups and conversation containers have their own future milestones.
"""

import uuid

import pytest
from plamenu_e2e import config, mitra, unique
from plamenu_e2e.api import Api
from plamenu_e2e.media import cached_attachment, make_png
from plamenu_e2e.steps import step, wait_for

ERIN = f"erin@{config.MITRA_DOMAIN}"


def _erin_follows(mitra_erin: Api, acct: str) -> dict:
    account = mitra_erin.resolve_account(acct)
    assert account, f"Mitra cannot resolve {acct}"
    mitra_erin.follow(account["id"])
    wait_for(
        lambda: mitra_erin.relationship(account["id"])["following"],
        desc=f"Mitra follow of {acct} to be accepted",
    )
    return account


def _follow_erin(plamenu_api: Api) -> dict:
    account = plamenu_api.resolve_account(ERIN)
    assert account, "Plamenu cannot resolve erin@mitra.local"
    plamenu_api.follow(account["id"])
    wait_for(
        lambda: plamenu_api.relationship(account["id"])["following"],
        desc="Mitra Accept(Follow) to reach Plamenu",
    )
    return account


@pytest.mark.federation(direction="both")
def test_discovery_both_directions(mitra_erin, plamenu_user, plamenu_api):
    with step(f"Mitra resolves @{plamenu_user.acct}"):
        account = mitra_erin.resolve_account(plamenu_user.acct)
        assert account and account["acct"] == plamenu_user.acct

    with step(f"Plamenu resolves @{ERIN}"):
        account = plamenu_api.resolve_account(ERIN)
        assert account and account["acct"] == ERIN


@pytest.mark.federation(direction="both")
def test_follow_and_unfollow_both_directions(mitra_erin, plamenu_user, plamenu_api, db):
    with step("Mitra follows Plamenu; signed Follow and Accept complete"):
        remote = _erin_follows(mitra_erin, plamenu_user.acct)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="Mitra follower row on Plamenu",
        )

    with step("Mitra Undo(Follow) removes the row"):
        mitra_erin.unfollow(remote["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="Mitra follower row to disappear",
        )

    with step("Plamenu follows Mitra and receives Accept"):
        erin = _follow_erin(plamenu_api)

    with step("Plamenu Undo(Follow) reaches Mitra"):
        # Mitra reports the remote follower as user@plamenu.local.
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        my_acct = f"{me['username']}@{config.PLAMENU_DOMAIN}"
        erin_id = mitra_erin.get("/api/v1/accounts/verify_credentials")["id"]

        def erin_has_follower() -> bool:
            return any(row["acct"] == my_acct for row in mitra_erin.followers(erin_id))

        wait_for(erin_has_follower, desc="Plamenu follower row on Mitra")
        plamenu_api.unfollow(erin["id"])
        wait_for(
            lambda: not erin_has_follower(),
            desc="Plamenu follower to disappear from Mitra",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_post_edit_delete_reach_mitra"
)
def test_mitra_post_edit_delete_reach_plamenu(mitra_erin, plamenu_api, marker):
    with step("Plamenu follows Mitra"):
        _follow_erin(plamenu_api)

    with step("Mitra Create(Note) reaches the Plamenu home timeline"):
        posted = mitra_erin.post_status(f"hello from mitra {marker}")
        got = wait_for(
            lambda: plamenu_api.home_status_containing(marker),
            desc="Mitra post on Plamenu home timeline",
        )

    with step("Mitra Update(Note) rewrites Plamenu's copy"):
        edited = unique("mitraedit")
        mitra_erin.edit_status(posted["id"], f"edited by mitra {edited}")
        wait_for(
            lambda: (
                edited
                in (plamenu_api.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="Mitra edit to reach Plamenu",
        )

    with step("Mitra Delete(Note) removes Plamenu's copy"):
        mitra_erin.delete_status(posted["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(got["id"]) is None,
            desc="Mitra delete to reach Plamenu",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mitra_post_edit_delete_reach_plamenu"
)
def test_plamenu_post_edit_delete_reach_mitra(mitra_erin, plamenu_api, marker):
    with step("Mitra follows the Plamenu author"):
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        _erin_follows(mitra_erin, f"{me['username']}@{config.PLAMENU_DOMAIN}")

    with step("Plamenu Create(Note) is ingested by Mitra"):
        posted = plamenu_api.post_status(f"hello from plamenu {marker}")
        got = wait_for(
            lambda: mitra.known_status(mitra_erin, posted["uri"]),
            desc="Plamenu post to be ingested by Mitra",
        )

    with step("Plamenu Update(Note) reaches Mitra"):
        edited = unique("plamedit")
        plamenu_api.edit_status(posted["id"], f"edited by plamenu {edited}")
        wait_for(
            lambda: (
                edited
                in (mitra_erin.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="Plamenu edit to reach Mitra",
        )

    with step("Plamenu Delete(Note) removes Mitra's copy"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: mitra_erin.get_status_or_none(got["id"]) is None,
            desc="Plamenu delete to reach Mitra",
        )


@pytest.mark.federation(direction="both")
def test_plamenu_quote_of_mitra_post(
    mitra_erin, alice, plamenu_user, plamenu_api, marker
):
    _follow_erin(plamenu_api)
    _erin_follows(mitra_erin, plamenu_user.acct)

    origin = mitra_erin.post_status(f"quote target {marker}")

    mastodon_target = wait_for(
        lambda: alice.resolve_status(origin["uri"]),
        desc="Mastodon to resolve Mitra's quote target",
    )
    mastodon_quote = alice.post_status(
        f"Mastodon quoting Mitra {marker}",
        quoted_status_id=mastodon_target["id"],
    )
    wait_for(
        lambda: alice.quote_state(mastodon_quote["id"]) == "accepted",
        desc="Mitra to authorize Mastodon's quote",
    )

    target = wait_for(
        lambda: plamenu_api.home_status_containing(marker),
        desc="Mitra quote target to reach Plamenu",
    )
    quote = plamenu_api.post_status(
        f"quoting Mitra {marker}", quoted_status_id=target["id"]
    )
    wait_for(
        lambda: plamenu_api.quote_state(quote["id"]) == "accepted",
        desc="Mitra to authorize the quote",
    )
    local = plamenu_api.get_status(quote["id"])
    assert local["quote"]["quoted_status"]["uri"] == origin["uri"]

    remote = wait_for(
        lambda: mitra.known_status(mitra_erin, quote["uri"]),
        desc="Mitra to ingest the quote",
    )
    assert remote["quote"]["quoted_status"]["uri"] == origin["uri"]

    mastodon_copy = wait_for(
        lambda: alice.resolve_status(quote["uri"]),
        desc="Mastodon to ingest a quote of the Mitra post",
    )
    wait_for(
        lambda: alice.quote_state(mastodon_copy["id"]) == "accepted",
        desc="Mastodon to verify Mitra's static authorization",
    )

    reverse_marker = f"{marker}-mitra-quote"
    mitra_erin.post_status(
        f"Mitra quoting Mitra {reverse_marker}", quote_id=origin["id"]
    )
    reverse = wait_for(
        lambda: plamenu_api.home_status_containing(reverse_marker),
        desc="Mitra quote to reach Plamenu",
    )
    wait_for(
        lambda: plamenu_api.quote_state(reverse["id"]) == "accepted",
        desc="Plamenu to verify Mitra's static authorization",
    )
    shown = plamenu_api.get_status(reverse["id"])
    assert shown["quote"]["quoted_status"]["uri"] == origin["uri"]


@pytest.mark.federation(direction="both")
def test_favourite_and_boost_both_directions(mitra_erin, plamenu_api, marker):
    with step("Mitra favourites and boosts a Plamenu post"):
        posted = plamenu_api.post_status(f"interact from mitra {marker}")
        remote = wait_for(
            lambda: mitra_erin.resolve_status(posted["uri"]),
            desc="Mitra to resolve Plamenu post",
        )
        mitra_erin.favourite(remote["id"])
        mitra_erin.reblog(remote["id"])
        wait_for(
            lambda: plamenu_api.notifications_from(ERIN, "favourite"),
            desc="Mitra favourite notification on Plamenu",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(ERIN, "reblog"),
            desc="Mitra boost notification on Plamenu",
        )

    with step("Plamenu favourites and boosts a Mitra post"):
        origin = mitra_erin.post_status(f"interact from plamenu {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(origin["uri"]),
            desc="Plamenu to resolve Mitra post",
        )
        plamenu_api.favourite(local["id"])
        plamenu_api.reblog(local["id"])
        # Mitra reports the remote actor as user@plamenu.local.
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        acct = f"{me['username']}@{config.PLAMENU_DOMAIN}"
        wait_for(
            lambda: mitra_erin.notifications_from(acct, "favourite"),
            desc="Plamenu favourite notification on Mitra",
        )
        wait_for(
            lambda: mitra_erin.notifications_from(acct, "reblog"),
            desc="Plamenu boost notification on Mitra",
        )


@pytest.mark.federation(direction="both")
def test_poll_votes_both_directions(mitra_erin, plamenu_api, marker):
    with step("Mitra votes on a Plamenu poll"):
        posted = plamenu_api.post_poll(f"Plamenu poll {marker}", ["one", "two"])
        remote = wait_for(
            lambda: mitra_erin.resolve_status(posted["uri"]),
            desc="Mitra to resolve Plamenu poll",
        )
        mitra_erin.vote(remote["poll"]["id"], [0])
        wait_for(
            lambda: plamenu_api.get_poll(posted["poll"]["id"])["votes_count"] == 1,
            desc="Mitra vote on Plamenu tally",
        )

    with step("Plamenu votes on a Mitra poll"):
        origin = mitra_erin.post_poll(f"Mitra poll {marker}", ["left", "right"])
        local = wait_for(
            lambda: plamenu_api.resolve_status(origin["uri"]),
            desc="Plamenu to resolve Mitra poll",
        )
        plamenu_api.vote(local["poll"]["id"], [1])
        # Mitra has no GET /api/v1/polls/{id}; the tally lives on the status.
        wait_for(
            lambda: mitra_erin.get_status(origin["id"])["poll"]["votes_count"] == 1,
            desc="Plamenu vote on Mitra tally",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound group lifecycle: Plamenu follows a Mitra group and ingests its wrapped Announce lifecycle; no outbound counterpart in this file.",
)
def test_group_follow_and_wrapped_announce_lifecycle(mitra_erin, plamenu_api, marker):
    """Plamenu follows a Mitra-hosted group (FEP-1b12) and consumes the
    wrapped-Announce dialect — Create arrives group-boosted, Update rewrites
    the copy, Delete removes it, Undo(Follow) leaves.

    A Plamenu post *to* the group is deliberately not tested: Mitra's inbound
    group detection resolves remote actors only (`ActorIdResolver::only_remote`
    in its note handler), so it never re-announces a remote author's post to
    its own group. Group participation from remote authors is a Mitra-side
    limitation, not a Plamenu one.
    """
    group_name = unique("plamgroup")
    with step("erin creates a group on Mitra"):
        group = mitra_erin.create_group(group_name, "Plamenu e2e group")

    with step("Plamenu resolves the group actor and follows it"):
        acct = f"{group_name}@{config.MITRA_DOMAIN}"
        remote = plamenu_api.resolve_account(acct)
        assert remote, f"Plamenu cannot resolve {acct}"
        assert remote["group"] is True, remote
        plamenu_api.follow(remote["id"])
        wait_for(
            lambda: plamenu_api.relationship(remote["id"])["following"],
            desc="group Accept(Follow) to reach Plamenu",
        )

    with step("erin's group post arrives as a boost by the group"):
        posted = mitra_erin.post_to_group(group["id"], f"group hello {marker}")
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="group-announced post on the Plamenu home timeline",
        )
        assert boost["account"]["acct"] == acct, boost["account"]
        assert boost["account"]["group"] is True
        inner = boost["reblog"]
        assert inner["account"]["acct"] == ERIN, inner["account"]

    with step("erin's edit reaches the announced copy (wrapped Update)"):
        edited = unique("groupedit")
        mitra_erin.edit_status(posted["id"], f"group edited {edited}")
        wait_for(
            lambda: (
                edited
                in (plamenu_api.get_status_or_none(inner["id"]) or {}).get(
                    "content", ""
                )
            ),
            desc="group-wrapped Update to reach Plamenu",
        )

    with step("erin's delete removes the copy (wrapped Delete)"):
        mitra_erin.delete_status(posted["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(inner["id"]) is None,
            desc="group-wrapped Delete to reach Plamenu",
        )

    with step("Plamenu leaves the group; erin dissolves it"):
        plamenu_api.unfollow(remote["id"])
        wait_for(
            lambda: not plamenu_api.relationship(remote["id"])["following"],
            desc="Undo(Follow) of the group",
        )
        mitra_erin.delete_group(group["id"])


@pytest.mark.federation(direction="both")
def test_reply_and_mention_both_directions(
    mitra_erin, plamenu_user, plamenu_api, db, marker
):
    """Replies + mentions both ways. Mitra delivers a reply/mention to a
    non-follower (the parent author is added to the primary audience) and
    serialises both Reply and Mention notifications as Mastodon type
    `mention`."""
    with step("Plamenu resolves erin's post and replies, mentioning erin"):
        posted = mitra_erin.post_status(f"reply to me from mitra {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="Plamenu to resolve erin's post",
        )
        reply = plamenu_api.post_status(
            f"@{ERIN} a reply from plamenu {marker}r", in_reply_to_id=local["id"]
        )
        assert reply["in_reply_to_id"] == local["id"]
        assert ERIN in [m["acct"] for m in reply["mentions"]]

    with step("erin gets a mention notification threaded onto her post"):
        plamenu_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        notifs = wait_for(
            lambda: mitra_erin.notifications_from(plamenu_acct, "mention"),
            desc="a mention notification from the plamenu user on Mitra",
        )
        assert notifs[0]["status"]["in_reply_to_id"] == posted["id"]

    with step("her replies_count and context reflect the reply"):
        wait_for(
            lambda: mitra_erin.get_status(posted["id"])["replies_count"] >= 1,
            desc="erin's replies_count to reach 1",
        )
        assert any(
            f"{marker}r" in s["content"]
            for s in mitra_erin.context(posted["id"])["descendants"]
        )

    with step("Plamenu posts; erin resolves it (author becomes known)"):
        local2 = plamenu_api.post_status(f"reply to me from plamenu {marker}2")
        remote = wait_for(
            lambda: mitra_erin.resolve_status(local2["uri"]),
            desc="Mitra to resolve the Plamenu status",
        )

    with step("erin replies without typing the plamenu author's handle"):
        reply = mitra_erin.post_status(
            f"a reply from mitra {marker}2r",
            in_reply_to_id=remote["id"],
        )
        assert plamenu_user.acct in [m["acct"] for m in reply["mentions"]], (
            "Mitra should add the parent author as a real Mention tag itself"
        )

    with step("the reply arrives threaded under the plamenu post"):
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}2r"),
            desc="Mitra reply in plamenu statuses",
        )
        assert db.status_parent_id(reply_id) == int(local2["id"])

    with step("the author gets a mention notification carrying the reply"):
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(ERIN, "mention"),
            desc="a mention notification from erin on Plamenu",
        )
        assert notifs[0]["status"]["id"] == str(reply_id)
        assert plamenu_user.username in [
            m["acct"] for m in notifs[0]["status"]["mentions"]
        ]
        assert plamenu_api.get_status(local2["id"])["replies_count"] >= 1


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="Mitra's normal API adds a Mention tag to replies, so its real signer CLI is used to reproduce the Incise-style top-level personalized-delivery shape.",
)
def test_to_addressed_non_reply_from_mitra_does_not_notify(
    mitra_erin, plamenu_user, plamenu_api, db, marker
):
    """A real peer-signed top-level Note may name the inbox owner in `to` for
    delivery without mentioning them. It is ingested with silent audience
    access but must not produce a false "mentioned you" notification."""
    with step("Mitra learns the Plamenu recipient"):
        _erin_follows(mitra_erin, plamenu_user.acct)
        recipient = db.local_actor_uri(plamenu_user.username)
        assert recipient, "the fresh Plamenu user's canonical actor ID is missing"

    with step("Mitra signs and sends an Incise-style personalized public Note"):
        object_id = f"{config.MITRA_URL}/objects/{uuid.uuid4()}"
        actor_id = f"{config.MITRA_URL}/users/{mitra.ERIN_NICK}"
        activity = {
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": f"{object_id}/activity",
            "type": "Create",
            "actor": actor_id,
            "to": [recipient, "https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": object_id,
                "type": "Note",
                "attributedTo": actor_id,
                "content": f"<p>personalized delivery {marker}</p>",
                "to": [recipient, "https://www.w3.org/ns/activitystreams#Public"],
                "cc": [f"{actor_id}/followers"],
            },
        }
        result = mitra.send_activity_rfc9421(activity, recipient)
        assert "202" in result, f"Plamenu rejected Mitra's signed Create: {result!r}"
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the personalized top-level Note to be ingested",
        )
        assert db.status_parent_id(status_id) is None

    with step("the audience remains silent and creates no mention notification"):
        stored = plamenu_api.get_status(str(status_id))
        assert stored["mentions"] == [], stored["mentions"]
        false_mentions = [
            notification
            for notification in plamenu_api.notifications_from(ERIN, "mention")
            if notification.get("status", {}).get("id") == str(status_id)
        ]
        assert false_mentions == [], false_mentions


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="Mitra cannot originate a Block (no client endpoint, no Block builder per FEDERATION.md), so there is no inbound Mitra-block counterpart; only the outbound Plamenu->Mitra block is observable.",
)
def test_block_federates_to_mitra(mitra_erin, plamenu_user, plamenu_api, db, marker):
    """Outbound Block: Plamenu blocks erin, severing the mutual follow and
    auto-Rejecting a re-follow; unblock lets erin follow again.

    The inbound reverse is a documented upstream gap: Mitra 5.7.0 exposes no
    block endpoint (POST /accounts/{id}/block 404s — only mute/unmute) and never
    emits a Block activity, so there is no way for erin to originate a block
    toward Plamenu. Plamenu's inbound Block handling is covered against Mastodon
    (test_blocks.py::test_block_federates_from_mastodon)."""
    with step("wire the mutual follow"):
        plamenu_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        erin = _follow_erin(plamenu_api)
        _erin_follows(mitra_erin, plamenu_acct)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="erin follower row on Plamenu",
        )
        plamenu_id = mitra_erin.resolve_account(plamenu_acct)["id"]

    with step("block erin through the client API; the follow severs both ways"):
        rel = plamenu_api.block(erin["id"])
        assert rel["blocking"] and not rel["following"] and not rel["followed_by"], rel
        assert [b["acct"] for b in plamenu_api.blocks()] == [ERIN]
        assert db.follower_count(plamenu_user.username) == 0

    with step("Mitra observes the severing"):
        wait_for(
            lambda: not mitra_erin.relationship(plamenu_id)["following"],
            desc="erin's follow of the plamenu user to be severed on Mitra",
        )

    with step("a re-follow from erin bounces off Plamenu's block"):
        mitra_erin.follow(plamenu_id)
        wait_for(
            lambda: (
                not mitra_erin.relationship(plamenu_id)["following"]
                and not mitra_erin.relationship(plamenu_id)["requested"]
            ),
            desc="Plamenu to Reject the re-follow",
        )
        assert db.follower_count(plamenu_user.username) == 0

    with step("unblock: Undo(Block) lets erin follow again"):
        rel = plamenu_api.unblock(erin["id"])
        assert not rel["blocking"] and plamenu_api.blocks() == []
        mitra_erin.follow(plamenu_id)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="erin's follow to be accepted again after unblock",
        )


@pytest.mark.federation(direction="both")
def test_profile_update_both_directions(
    mitra_erin, plamenu_user, plamenu_api, db, marker
):
    """Profile Update(Actor) both ways: display name + note round-trip.

    (Avatar is intentionally omitted — Mitra's multipart avatar field is
    unverified and its serializer carries no focal/blurhash; display_name + note
    are the reliable Update(Actor) signals.)"""
    with step("erin follows the Plamenu author so Updates reach Mitra"):
        plamenu_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        account = _erin_follows(mitra_erin, plamenu_acct)
        remote_id = account["id"]

    with step("edit the profile through Plamenu's client API"):
        new_name = f"Renamed {marker}"
        entity = plamenu_api.update_profile(display_name=new_name, note=f"bio {marker}")
        assert entity["display_name"] == new_name and f"bio {marker}" in entity["note"]

    with step("Mitra applies the Update(Actor)"):
        # account(remote_id) reads the stored (Update-applied) copy; search would
        # re-dereference and mask a federation failure.
        wait_for(
            lambda: mitra_erin.account(remote_id)["display_name"] == new_name,
            desc="the new display name to reach Mitra",
        )
        assert marker in mitra_erin.account(remote_id)["note"]

    with step("Plamenu follows erin"):
        _follow_erin(plamenu_api)

    with step("erin edits her profile on Mitra"):
        erin_name = f"Erin {marker}"
        mitra_erin.update_profile(display_name=erin_name)

    with step("Plamenu applies the Update(Actor)"):
        wait_for(
            lambda: db.remote_display_name("erin", config.MITRA_DOMAIN) == erin_name,
            desc="erin's new display name to reach Plamenu",
        )


@pytest.mark.federation(direction="both")
def test_image_attachment_both_directions(
    mitra_erin, plamenu_user, plamenu_api, db, marker
):
    """Image attachments both ways: alt text round-trips; Plamenu recomputes its
    own blurhash/dimensions on ingest and proxies the file onto its /media/
    route. Mitra federates no blurhash/focalPoint, so those are not asserted on
    Mitra's copy and focus_x/y are asserted NULL on the inbound row."""
    with step("erin follows the Plamenu author"):
        plamenu_acct = f"{plamenu_user.username}@{config.PLAMENU_DOMAIN}"
        _erin_follows(mitra_erin, plamenu_acct)

    with step("Plamenu uploads an image with description + focus and posts it"):
        up = plamenu_api.upload_media(
            make_png(),
            filename="pic.png",
            mime="image/png",
            description="an e2e picture",
            focus="-0.5,0.3",
        )
        assert up["blurhash"] and up["meta"]["focus"]["x"] == -0.5
        posted = plamenu_api.post_with_media(f"a picture {marker}", [up["id"]])

    with step("Mitra ingests the attachment (type + alt text)"):
        got = wait_for(
            lambda: mitra.known_status(mitra_erin, posted["uri"]),
            desc="Plamenu media post on erin's home timeline",
        )
        att = got["media_attachments"][0]
        assert att["type"] == "image"
        assert att["description"] == "an e2e picture"

    with step("erin uploads an image with description and posts it"):
        erin_up = mitra_erin.upload_media(
            make_png(rgb=(30, 30, 200)),
            filename="mitra.png",
            mime="image/png",
            description="a mitra picture",
        )
        mitra_erin.post_with_media(f"mitra media {marker}", [erin_up["id"]])

    with step("the stored Plamenu row carries the federated metadata"):
        # Match erin's post specifically — the Plamenu-authored post above
        # carries the same `marker` (with the "an e2e picture" attachment), so a
        # bare-marker lookup would grab the wrong status.
        status_id = wait_for(
            lambda: db.status_id_containing(f"mitra media {marker}"),
            desc="Mitra media post in plamenu statuses",
        )
        # The attachment row is inserted at ingest with the federated metadata,
        # then Plamenu's remote-media job downloads the file and backfills the
        # recomputed blurhash + dimensions asynchronously — so wait for the
        # blurhash to be present, not merely for the row to exist (it races the
        # backfill under load, though it wins in isolation).
        rows = wait_for(
            lambda: (
                [r for r in db.media_for_status(status_id) if r[2] and r[5] and r[6]]
                or None
            ),
            desc="attachment row stored with Plamenu's recomputed blurhash + dimensions",
        )
        ct, descr, blurhash, fx, fy, w, h = rows[0]
        assert ct.startswith("image/"), ct
        assert descr == "a mitra picture", descr
        assert blurhash, "Plamenu should recompute a blurhash"
        assert w and h, (w, h)
        assert fx is None and fy is None, "Mitra federates no focalPoint"

    with step("Plamenu proxies the image from its own /media/ route"):
        att = wait_for(
            lambda: cached_attachment(plamenu_api, status_id),
            desc="attachment cached and served locally",
        )
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/"), att["url"]
        assert config.MITRA_DOMAIN not in (att.get("url") or "")


@pytest.mark.skip(
    reason="Mitra 5.7.0 has no reports endpoint (POST /api/v1/reports 404s) and "
    "no Flag activity handler in either direction; Plamenu's Flag emit + ingest "
    "are covered by test_reports.py against Mastodon."
)
@pytest.mark.federation(
    direction="outbound",
    one_way_reason="Mitra 5.7.0 has no reports endpoint (POST /api/v1/reports 404) and no Flag activity handler in either direction, so neither an outbound nor an inbound report is observable; skipped. Plamenu's Flag emit+ingest are covered by test_reports.py.",
)
def test_report_flag_unsupported(mitra_erin, plamenu_api):
    """Documented upstream gap: forwarded Flag/reports are not exercisable
    against Mitra (no reports API, no Flag activity)."""
