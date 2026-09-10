"""Baseline bidirectional federation between Plamenu and Sharkey (Misskey
family, sharkey.local).

Emoji reactions have their own module (test_sharkey_reactions.py); this proves
the ordinary microblogging surface both ways against the Misskey dialect —
discovery, follow/unfollow, note Create/Update/Delete, reply threading,
renote(=boost)/Undo, poll ingestion + voting, profile Update, and image
attachments. Sharkey speaks the Misskey API (bare JSON POSTs), so the peer side
goes through the `sharkey_carol` fixture (a Sharkey client, not an `Api`); the
peer is optional and skips when the instance is down.

carol is the standing root/admin account — she is unlocked (auto-accepts
follows). Tests that rename her use a per-test marker so they stay independent.
"""

import pytest
from plamenu_e2e import config, unique
from plamenu_e2e.media import make_png
from plamenu_e2e.sharkey import SharkeyError
from plamenu_e2e.steps import log, step, wait_for

CAROL = f"carol@{config.SHARKEY_DOMAIN}"


def _actor_url(plamenu_api, username: str) -> str:
    """Resolve the human profile route to the persisted canonical actor ID."""
    return plamenu_api.ap_get(f"/users/{username}")["id"]


def _ap_note(sharkey_carol, uri: str):
    """ap/show a note by URI, returning None (not raising) until it resolves —
    for use inside `wait_for`."""
    try:
        shown = sharkey_carol.resolve(uri)
    except SharkeyError:
        return None
    return shown if shown.get("type") == "Note" else None


def _carol_view_of(sharkey_carol, plamenu_api, plamenu_user) -> str:
    """The sharkey-local user id for a Plamenu account (resolved via ap/show)."""
    shown = sharkey_carol.resolve(_actor_url(plamenu_api, plamenu_user.username))
    assert shown["type"] == "User", shown
    return shown["object"]["id"]


# ── discovery ─────────────────────────────────────────────────────────


@pytest.mark.federation(direction="both")
def test_sharkey_discovery_both_directions(sharkey_carol, plamenu_user, plamenu_api):
    with step(f"Plamenu resolves @{CAROL} (webfinger + actor fetch)"):
        account = plamenu_api.resolve_account(CAROL)
        assert account and account["acct"] == CAROL, account

    with step("carol resolves the fresh Plamenu user via ap/show"):
        actor_url = _actor_url(plamenu_api, plamenu_user.username)
        shown = sharkey_carol.resolve(actor_url)
        assert shown["type"] == "User", shown
        obj = shown["object"]
        assert obj["username"] == plamenu_user.username, obj
        assert obj["host"] == config.PLAMENU_DOMAIN, obj
        assert obj["uri"] == actor_url, obj


# ── follow ────────────────────────────────────────────────────────────


@pytest.mark.federation(direction="both")
def test_follow_unfollow_between_plamenu_and_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db
):
    with step("carol follows the Plamenu user (inbound Follow -> auto-Accept)"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to appear as a follower on Plamenu",
        )
        wait_for(
            lambda: sharkey_carol.show_user(
                plamenu_user.username, host=config.PLAMENU_DOMAIN
            )["isFollowing"],
            desc="Sharkey to commit the accepted following relationship",
        )

    with step("carol unfollows -> Undo(Follow) removes the row"):
        sharkey_carol.unfollow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the follow row to disappear after Undo",
        )

    with step("the Plamenu user follows carol (outbound Follow -> Accept)"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )
        wait_for(
            lambda: sharkey_carol.show_user(
                plamenu_user.username, host=config.PLAMENU_DOMAIN
            )["isFollowed"],
            desc="isFollowed to become true on the Sharkey side",
        )

    with step("the Plamenu user unfollows carol"):
        carol_local = plamenu_api.resolve_account(CAROL)
        plamenu_api.unfollow(carol_local["id"])
        wait_for(
            lambda: (
                not sharkey_carol.show_user(
                    plamenu_user.username, host=config.PLAMENU_DOMAIN
                )["isFollowed"]
            ),
            desc="isFollowed to drop back to false",
        )


# ── notes: create / delete / edit ─────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_note_create_and_delete_federate_from_sharkey"
)
def test_note_create_and_delete_federate_to_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("carol follows the Plamenu user so posts are delivered"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to follow",
        )

    with step("Plamenu posts; the note reaches carol's timeline"):
        local = plamenu_api.post_status(f"hello sharkey {marker}", visibility="public")
        wait_for(
            lambda: sharkey_carol.timeline_note_containing(marker),
            desc="the Plamenu post on carol's home timeline",
        )

    with step("Plamenu deletes; Delete(Note) removes carol's copy"):
        plamenu_api.delete_status(local["id"])
        wait_for(
            lambda: sharkey_carol.timeline_note_containing(marker) is None,
            desc="the deleted post to leave carol's timeline",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_note_create_and_delete_federate_to_sharkey"
)
def test_note_create_and_delete_federate_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("carol posts; the note arrives in Plamenu"):
        note = sharkey_carol.post_note(f"hello plamenu {marker}")
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="carol's post to arrive in Plamenu's statuses table",
        )
        log(f"stored as status {status_id}")

    with step("carol deletes; the row disappears"):
        sharkey_carol.delete_note(note["id"])
        wait_for(
            lambda: db.status_id_containing(marker) is None,
            desc="the deleted status row to disappear from Plamenu",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_note_edit_federates_from_sharkey"
)
def test_note_edit_federates_to_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("carol follows the Plamenu user (Update goes to the Create audience)"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to follow",
        )

    with step("Plamenu posts; carol resolves her delivered copy"):
        local = plamenu_api.post_status(f"original wording {marker}")
        note = wait_for(
            lambda: sharkey_carol.timeline_note_containing(marker),
            desc="the post to reach carol",
        )
        note_id = note["id"]

    with step("Plamenu edits; Update(Note) reaches carol"):
        edited = unique("edited")
        plamenu_api.edit_status(local["id"], f"corrected wording {edited}")
        wait_for(
            lambda: edited in (sharkey_carol.show_note(note_id).get("text") or ""),
            desc="the edited content to replace the original on Sharkey",
        )
        assert sharkey_carol.show_note(note_id).get("updatedAt") is not None


@pytest.mark.federation(
    direction="inbound", reverse_of="test_note_edit_federates_to_sharkey"
)
def test_note_edit_federates_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("carol posts; the note reaches Plamenu"):
        note = sharkey_carol.post_note(f"original wording {marker}")
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="carol's post to arrive in Plamenu",
        )

    with step("carol edits via notes/edit; Plamenu applies the Update(Note)"):
        edited = unique("edited")
        sharkey_carol.edit_note(note["id"], f"corrected wording {edited}")
        wait_for(
            lambda: db.status_id_containing(edited) == status_id,
            desc="the edited content to replace the original in place",
        )
        assert db.status_edited_at(status_id) is not None
        assert db.status_id_containing(marker) is None


# ── replies ───────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_reply_federates_from_sharkey"
)
def test_reply_federates_to_sharkey(sharkey_carol, plamenu_user, plamenu_api, marker):
    with step("carol posts a note"):
        note = sharkey_carol.post_note(f"reply to me from plamenu {marker}")
        note_uri = f"{sharkey_carol.base_url}/notes/{note['id']}"

    with step("Plamenu resolves and replies, mentioning carol"):
        local = wait_for(
            lambda: plamenu_api.resolve_status(note_uri),
            desc="Plamenu to resolve carol's note",
        )
        reply = plamenu_api.post_status(
            f"@{CAROL} a reply {marker}r", in_reply_to_id=local["id"]
        )
        assert reply["in_reply_to_id"] == local["id"]
        assert CAROL in [m["acct"] for m in reply["mentions"]]

    with step("the reply threads under carol's note"):
        child = wait_for(
            lambda: next(
                (
                    n
                    for n in sharkey_carol.children(note["id"])
                    if f"{marker}r" in (n.get("text") or "")
                ),
                None,
            ),
            desc="the reply to thread under carol's note",
        )
        assert child["replyId"] == note["id"]


@pytest.mark.federation(
    direction="inbound", reverse_of="test_reply_federates_to_sharkey"
)
def test_reply_federates_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, db, marker
):
    with step("Plamenu posts; carol resolves it"):
        local = plamenu_api.post_status(f"reply to me from sharkey {marker}")
        shown = wait_for(
            lambda: _ap_note(sharkey_carol, local["uri"]),
            desc="carol to resolve the Plamenu status",
        )
        parent_note_id = shown["object"]["id"]

    with step("carol replies to it"):
        sharkey_carol.reply(parent_note_id, f"a reply from sharkey {marker}r")

    with step("the reply arrives threaded under the Plamenu post"):
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="carol's reply to arrive in Plamenu",
        )
        assert db.status_parent_id(reply_id) == int(local["id"])

    with step("the author gets a mention notification and context reflects it"):
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(CAROL, "mention"),
            desc="a mention notification from carol",
        )
        assert notifs[0]["status"]["id"] == str(reply_id)
        assert plamenu_api.get_status(local["id"])["replies_count"] >= 1
        assert any(
            f"{marker}r" in s["content"]
            for s in plamenu_api.context(local["id"])["descendants"]
        )


# ── renote (boost) ────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_sharkey_renote_federates_to_plamenu"
)
def test_plamenu_boost_surfaces_as_renote_on_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, marker
):
    with step("carol posts a note"):
        note = sharkey_carol.post_note(f"boost me from plamenu {marker}")
        note_uri = f"{sharkey_carol.base_url}/notes/{note['id']}"

    with step("Plamenu resolves and boosts it"):
        local = wait_for(
            lambda: plamenu_api.resolve_status(note_uri),
            desc="Plamenu to resolve carol's note",
        )
        plamenu_api.reblog(local["id"])

    with step("the Announce shows up as a renote by the Plamenu user"):
        wait_for(
            lambda: any(
                r["user"]["host"] == config.PLAMENU_DOMAIN
                and r["user"]["username"] == plamenu_user.username
                for r in sharkey_carol.renotes(note["id"])
            ),
            desc="Plamenu's renote to appear in notes/renotes",
        )

    with step("Plamenu unboosts; Undo(Announce) removes the renote"):
        plamenu_api.unreblog(local["id"])
        wait_for(
            lambda: (
                not any(
                    r["user"]["host"] == config.PLAMENU_DOMAIN
                    for r in sharkey_carol.renotes(note["id"])
                )
            ),
            desc="the renote to disappear after Undo",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_boost_surfaces_as_renote_on_sharkey"
)
def test_sharkey_renote_federates_to_plamenu(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol so the renote fans out"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("Plamenu posts; carol resolves it"):
        local = plamenu_api.post_status(f"boost me from sharkey {marker}")
        shown = wait_for(
            lambda: _ap_note(sharkey_carol, local["uri"]),
            desc="carol to resolve the Plamenu status",
        )
        note_id = shown["object"]["id"]

    with step("carol renotes it (pure renote)"):
        sharkey_carol.renote(note_id)
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] >= 1,
            desc="the Announce to register on Plamenu",
        )

    with step("the boost is attributed to carol"):
        assert CAROL in [a["acct"] for a in plamenu_api.reblogged_by(local["id"])]
        assert db.reblog_count(int(local["id"])) == 1

    with step("carol unrenotes; Undo(Announce) clears it"):
        sharkey_carol.unrenote(note_id)
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["reblogs_count"] == 0,
            desc="the reblog count to drop back to 0",
        )
        assert plamenu_api.reblogged_by(local["id"]) == []


# ── polls ─────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_poll_federates_from_sharkey"
)
def test_poll_federates_to_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("carol follows the Plamenu user so the poll is delivered"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to follow",
        )

    with step("Plamenu posts a poll"):
        entity = plamenu_api.post_poll(f"pick one {marker}", ["tea", "coffee"])
        local_poll_id = entity["poll"]["id"]

    with step("carol resolves the poll and sees the options"):
        note = wait_for(
            lambda: sharkey_carol.timeline_note_containing(marker),
            desc="the poll to reach carol",
        )
        poll = sharkey_carol.poll_of(note["id"])
        assert [c["text"] for c in poll["choices"]] == ["tea", "coffee"], poll

    with step("carol votes; the vote tallies on Plamenu's poll"):
        sharkey_carol.vote(note["id"], 0)
        wait_for(
            lambda: db.poll_tallies(int(local_poll_id)) == [1, 0],
            desc="carol's federated vote to tally on Plamenu",
        )
        assert plamenu_api.get_poll(local_poll_id)["votes_count"] == 1

    with step("the tally Update(Question) reaches Sharkey"):
        wait_for(
            lambda: sharkey_carol.poll_of(note["id"])["choices"][0]["votes"] == 1,
            desc="Sharkey's copy to show the refreshed tally",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_poll_federates_to_sharkey"
)
def test_poll_federates_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("carol posts a poll; Plamenu stores options + tallies"):
        note = sharkey_carol.poll_note(f"choose {marker}", ["red", "blue"])
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the poll to arrive in Plamenu",
        )
        stored = wait_for(lambda: db.poll_for_status(status_id), desc="the poll row")
        poll_id, options, tallies = stored
        assert options == ["red", "blue"]
        assert tallies == [0, 0]

    with step("the poll renders through Plamenu's client API"):
        rendered = plamenu_api.get_poll(str(poll_id))
        assert [o["title"] for o in rendered["options"]] == ["red", "blue"]
        assert rendered["expired"] is False

    with step("Plamenu votes; the origin poll on Sharkey tallies it"):
        plamenu_api.vote(str(poll_id), [1])
        wait_for(
            lambda: sharkey_carol.poll_of(note["id"])["choices"][1]["votes"] == 1,
            desc="the federated vote to tally on Sharkey's origin poll",
        )


# ── profile ───────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_profile_edit_federates_from_sharkey"
)
def test_profile_edit_federates_to_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, db, marker
):
    with step("carol follows the Plamenu user"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to follow",
        )

    with step("Plamenu edits its profile"):
        new_name = f"Renamed {marker}"
        plamenu_api.update_profile(
            display_name=new_name, note=f"bio {marker}", bot="true"
        )

    with step("Sharkey applies the Update(Actor)"):
        wait_for(
            lambda: (
                sharkey_carol.show_user(
                    plamenu_user.username, host=config.PLAMENU_DOMAIN
                )["name"]
                == new_name
            ),
            desc="the new display name to reach Sharkey",
        )
        u = sharkey_carol.show_user(plamenu_user.username, host=config.PLAMENU_DOMAIN)
        assert marker in (u.get("description") or ""), u
        assert u.get("isBot") is True, u


@pytest.mark.federation(
    direction="inbound", reverse_of="test_profile_edit_federates_to_sharkey"
)
def test_profile_edit_federates_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol (Updates reach us)"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("carol edits her profile"):
        new_name = f"Carol {marker}"
        sharkey_carol.update_profile(name=new_name, description=f"bio {marker}")

    with step("Plamenu applies the Update(Actor)"):
        wait_for(
            lambda: db.remote_display_name("carol", config.SHARKEY_DOMAIN) == new_name,
            desc="carol's new display name to reach Plamenu",
        )


# ── image attachments ─────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_image_attachment_federates_from_sharkey"
)
def test_image_attachment_federates_to_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("carol follows the Plamenu user"):
        carol_view = _carol_view_of(sharkey_carol, plamenu_api, plamenu_user)
        sharkey_carol.follow(carol_view)
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 1,
            desc="carol to follow",
        )

    with step("Plamenu uploads an image and posts it"):
        up = plamenu_api.upload_media(
            make_png(),
            filename="pic.png",
            mime="image/png",
            description="an e2e picture",
        )
        plamenu_api.post_with_media(f"a picture {marker}", [up["id"]])

    with step("carol receives the note with a downloaded image file"):
        note = wait_for(
            lambda: sharkey_carol.timeline_note_containing(marker),
            desc="the media post to reach carol",
        )
        full = wait_for(
            lambda: sharkey_carol.show_note(note["id"]).get("files") or None,
            desc="Sharkey to download the attached image",
        )
        assert full[0]["type"].startswith("image/"), full[0]


@pytest.mark.federation(
    direction="inbound", reverse_of="test_image_attachment_federates_to_sharkey"
)
def test_image_attachment_federates_from_sharkey(
    sharkey_carol, plamenu_user, plamenu_api, cli, db, marker
):
    with step("the Plamenu user follows carol"):
        cli.follow(plamenu_user.username, CAROL)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to be accepted",
        )

    with step("carol uploads an image to the drive and posts it"):
        file = sharkey_carol.upload(make_png(), "sharkey.png")
        sharkey_carol.post_note(f"sharkey media {marker}", fileIds=[file["id"]])

    with step("the attachment lands on Plamenu's stored row with a computed blurhash"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the media post to arrive in Plamenu",
        )
        # Plamenu inserts the row at ingest, then its remote-media job downloads
        # the file and backfills the recomputed blurhash + dimensions
        # asynchronously — wait for those (r[2]=blurhash, r[5]=width, r[6]=height),
        # not merely for the row to exist (it races the backfill under load).
        rows = wait_for(
            lambda: (
                [r for r in db.media_for_status(status_id) if r[2] and r[5] and r[6]]
                or None
            ),
            desc="the attachment row with recomputed blurhash + dimensions",
        )
        content_type, _description, blurhash, _fx, _fy, w, h = rows[0]
        assert content_type.startswith("image/"), content_type
        assert blurhash, "Plamenu should recompute a blurhash on download"
        assert w and h, (w, h)
