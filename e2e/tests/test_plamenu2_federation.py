"""Plamenu against Plamenu: the social core.

Every other peer in the fleet is foreign software, so every other federation
test measures this server against somebody else's subset of ActivityPub: what
Mastodon drops, what Pleroma renames, what Lemmy wraps. None of them can fail
on the half of a dialect that *nobody else implements* — a field we emit and
nobody reads looks exactly like a field we emit correctly.

These tests close that hole. Both sides are this server, so an assertion here
is about the wire and nothing else: what one instance says, the other must
understand, and the entity the reader ends up with must be the entity the
author published. The peer (`plamenu2`, see `plamenu_e2e/ephemeral.py`) is a
fresh install started for the test session from this same tree.

Direction is `both` throughout: with identical software on either side,
splitting a round trip into an "inbound" and an "outbound" test would assert
the same code twice under two names.
"""

import pytest
from plamenu_e2e import interop, shapes
from plamenu_e2e.interop import as_a_reader_rewrites_it
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(direction="both")
def test_follow_lifecycle_between_two_plamenu_instances(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """Discovery, the follow handshake and its undo, both ways at once.

    Covers: webfinger + actor fetch of a Plamenu actor by a Plamenu server,
    the resolved account entity (handle, profile URL, actor URI), Accept
    settling the relationship on the requester's side, the follower landing
    in the followee's counts and notifications, delivery to a fresh follower,
    and `Undo(Follow)` dropping the follower again."""
    with step("each instance resolves the other's fresh account"):
        theirs = interop.resolve(plamenu_api, plamenu2_user.acct)
        mine = interop.resolve(plamenu2_api, plamenu_user.acct)
        assert theirs["url"] == f"{plamenu2.url}/@{plamenu2_user.username}", theirs
        assert theirs["username"] == plamenu2_user.username
        assert theirs["display_name"] == "E2E Peer"
        assert mine["username"] == plamenu_user.username
        log(f"{theirs['acct']} <-> {mine['acct']}")

    with step("the peer's user follows the local one; the Accept settles it"):
        plamenu2_api.follow(mine["id"])
        wait_for(
            lambda: plamenu2_api.relationship(mine["id"])["following"],
            desc="the local instance's Accept(Follow) to reach the peer",
        )

    with step("the followee records the follower and is notified"):
        me = wait_for(
            lambda: (
                account
                if (account := plamenu_api.get("/api/v1/accounts/verify_credentials"))
                and account["followers_count"] == 1
                else None
            ),
            desc="the inbound follow to count on the followee's profile",
        )
        assert me["followers_count"] == 1
        followers = plamenu_api.followers(me["id"])
        assert [f["acct"] for f in followers] == [plamenu2_user.acct], followers
        wait_for(
            lambda: plamenu_api.notifications_from(plamenu2_user.acct, "follow"),
            desc="a follow notification for the followee",
        )

    with step("a post from the followee is delivered to the new follower"):
        posted = plamenu_api.post_status(f"first contact {marker}")
        got = interop.delivered(plamenu2_api, marker)
        assert got["uri"] == posted["uri"], got
        assert got["url"] == posted["url"], got
        assert got["account"]["acct"] == plamenu_user.acct
        assert got["content"] == posted["content"], (posted["content"], got["content"])

    with step("the follower unfollows; the followee loses the follower"):
        plamenu2_api.unfollow(mine["id"])
        wait_for(
            lambda: (
                plamenu_api.get("/api/v1/accounts/verify_credentials")[
                    "followers_count"
                ]
                == 0
            ),
            desc="the Undo(Follow) to drop the follower row",
        )
        assert plamenu2_api.relationship(mine["id"])["following"] is False


@pytest.mark.federation(direction="both")
def test_locked_account_holds_then_authorizes_a_remote_request(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A locked account on either side holds the other's Follow as a request.

    Covers: `manuallyApprovesFollowers` on the actor document as a peer reads
    it, the request held pending on both ends, `authorize` federating an
    `Accept` that flips the requester's relationship, and `reject` federating
    a `Reject` that clears it."""
    with step("both accounts lock themselves"):
        assert plamenu_api.update_profile(locked="true")["locked"] is True
        assert plamenu2_api.update_profile(locked="true")["locked"] is True

    with step("each side sees the other as locked"):
        theirs = interop.resolve(plamenu_api, plamenu2_user.acct)
        mine = interop.resolve(plamenu2_api, plamenu_user.acct)
        assert theirs["locked"] is True, theirs
        assert mine["locked"] is True, mine

    with step("the peer's follow is held as a request, not an acceptance"):
        plamenu2_api.follow(mine["id"])
        requests = wait_for(
            lambda: plamenu_api.get("/api/v1/follow_requests") or None,
            desc="the inbound Follow to be held as a request",
        )
        assert [r["acct"] for r in requests] == [plamenu2_user.acct], requests
        relationship = plamenu2_api.relationship(mine["id"])
        assert relationship["following"] is False, relationship
        assert relationship["requested"] is True, relationship
        wait_for(
            lambda: plamenu_api.notifications_from(
                plamenu2_user.acct, "follow_request"
            ),
            desc="a follow_request notification",
        )

    with step("authorizing federates the Accept and the requester follows"):
        plamenu_api.post(f"/api/v1/follow_requests/{requests[0]['id']}/authorize")
        wait_for(
            lambda: plamenu2_api.relationship(mine["id"])["following"],
            desc="the Accept(Follow) to settle the peer's relationship",
        )
        assert plamenu2_api.relationship(mine["id"])["requested"] is False

    with step("the reverse request is rejected and clears on the requester"):
        plamenu_api.follow(theirs["id"])
        held = wait_for(
            lambda: plamenu2_api.get("/api/v1/follow_requests") or None,
            desc="the peer to hold our Follow as a request",
        )
        assert [r["acct"] for r in held] == [plamenu_user.acct], held
        plamenu2_api.post(f"/api/v1/follow_requests/{held[0]['id']}/reject")
        wait_for(
            lambda: plamenu_api.relationship(theirs["id"])["requested"] is False,
            desc="the Reject(Follow) to clear the pending request",
        )
        assert plamenu_api.relationship(theirs["id"])["following"] is False


@pytest.mark.federation(direction="both")
def test_reply_threads_build_on_both_sides(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A conversation crossing the instance boundary twice.

    Covers: `inReplyTo` threading in both directions, the mention that makes
    a reply reach a non-follower, mention notifications, and both servers
    ending up with the same three-post thread in `/context` — ancestors and
    descendants, from either end of it."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user opens a thread"):
        root = plamenu_api.post_status(f"thread root {marker}")
        their_root = interop.delivered(plamenu2_api, f"thread root {marker}")

    with step("the peer replies, mentioning the author"):
        reply = plamenu2_api.post_status(
            f"@{plamenu_user.acct} first reply {marker}",
            in_reply_to_id=their_root["id"],
        )
        assert reply["in_reply_to_id"] == their_root["id"]
        our_reply = wait_for(
            lambda: plamenu_api.home_status_containing(f"first reply {marker}"),
            desc="the reply to arrive at the author's instance",
        )
        assert our_reply["in_reply_to_id"] == root["id"], our_reply
        assert our_reply["in_reply_to_account_id"] == root["account"]["id"]
        wait_for(
            lambda: plamenu_api.notifications_from(plamenu2_user.acct, "mention"),
            desc="a mention notification for the thread author",
        )

    with step("the author answers; the peer threads it under its own reply"):
        answer = plamenu_api.post_status(
            f"@{plamenu2_user.acct} last word {marker}",
            in_reply_to_id=our_reply["id"],
        )
        their_answer = wait_for(
            lambda: plamenu2_api.home_status_containing(f"last word {marker}"),
            desc="the answer to arrive at the peer",
        )
        assert their_answer["in_reply_to_id"] == reply["id"], their_answer

    with step("both instances hold the same three-post thread"):
        ours = plamenu_api.context(our_reply["id"])
        assert [s["uri"] for s in ours["ancestors"]] == [root["uri"]], ours
        assert [s["uri"] for s in ours["descendants"]] == [answer["uri"]], ours
        theirs = plamenu2_api.context(reply["id"])
        assert [s["uri"] for s in theirs["ancestors"]] == [root["uri"]], theirs
        assert [s["uri"] for s in theirs["descendants"]] == [answer["uri"]], theirs


@pytest.mark.federation(direction="both")
def test_boosts_and_favourites_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """Announce and Like, and the Undo of each, in both directions.

    Covers: the boost wrapper on the follower's timeline, the interaction
    counters and `reblogged_by`/`favourited_by` listings on the author's side,
    both notifications, and both undos taking the counters back down."""
    _theirs, _mine = interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user posts; the peer boosts and favourites it"):
        posted = plamenu_api.post_status(f"boost me {marker}")
        theirs_copy = interop.delivered(plamenu2_api, marker)
        plamenu2_api.reblog(theirs_copy["id"])
        plamenu2_api.favourite(theirs_copy["id"])

    with step("the author's counters, listings and notifications move"):
        wait_for(
            lambda: (
                plamenu_api.get_status(posted["id"])["reblogs_count"] == 1
                and plamenu_api.get_status(posted["id"])["favourites_count"] == 1
            ),
            desc="the Announce and Like to be counted by the author",
        )
        assert [a["acct"] for a in plamenu_api.reblogged_by(posted["id"])] == [
            plamenu2_user.acct
        ]
        assert [a["acct"] for a in plamenu_api.favourited_by(posted["id"])] == [
            plamenu2_user.acct
        ]
        wait_for(
            lambda: plamenu_api.notifications_from(plamenu2_user.acct, "reblog"),
            desc="a reblog notification",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(plamenu2_user.acct, "favourite"),
            desc="a favourite notification",
        )

    with step("undoing both takes the counters back to zero"):
        plamenu2_api.unreblog(theirs_copy["id"])
        plamenu2_api.unfavourite(theirs_copy["id"])
        wait_for(
            lambda: (
                plamenu_api.get_status(posted["id"])["reblogs_count"] == 0
                and plamenu_api.get_status(posted["id"])["favourites_count"] == 0
            ),
            desc="the two Undos to clear the counters",
        )

    with step("the reverse direction: our boost of the peer's post"):
        their_post = plamenu2_api.post_status(f"boost me back {marker}")
        ours = interop.delivered(plamenu_api, f"boost me back {marker}")
        boost = plamenu_api.reblog(ours["id"])
        assert boost["reblog"]["uri"] == their_post["uri"], boost
        wait_for(
            lambda: plamenu2_api.get_status(their_post["id"])["reblogs_count"] == 1,
            desc="our Announce to be counted by the peer",
        )
        wrapper = wait_for(
            lambda: plamenu2_api.home_reblog_containing(f"boost me back {marker}"),
            desc="the boost wrapper to reach the boosted author's own timeline",
        )
        assert wrapper["account"]["acct"] == plamenu_user.acct, wrapper
        assert wrapper["reblog"]["uri"] == their_post["uri"], wrapper
        plamenu_api.unreblog(ours["id"])
        wait_for(
            lambda: plamenu2_api.get_status(their_post["id"])["reblogs_count"] == 0,
            desc="our Undo(Announce) to reach the peer",
        )


@pytest.mark.federation(direction="both")
def test_emoji_reactions_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """`EmojiReact` and its Undo — the dialect only Pleroma-family peers share.

    Covers: the reaction landing in `emoji_reactions` (top level and under
    `pleroma`) with the reactor listed, the `pleroma:emoji_reaction`
    notification carrying the emoji, and the Undo clearing it — then the same
    the other way round."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the peer reacts to a local post"):
        posted = plamenu_api.post_status(f"react to me {marker}")
        theirs = interop.delivered(plamenu2_api, marker)
        plamenu2_api.react(theirs["id"], "🔥")

    with step("the reaction shows on the author's copy, with the reactor"):
        reaction = wait_for(
            lambda: next(
                (
                    r
                    for r in plamenu_api.emoji_reactions(posted["id"])
                    if r["name"] == "🔥"
                ),
                None,
            ),
            desc="the inbound EmojiReact to surface on the status",
        )
        assert reaction["count"] == 1, reaction
        assert reaction["me"] is False, reaction
        reactors = [
            plamenu_api.account(account_id)["acct"]
            for account_id in reaction["account_ids"]
        ]
        assert reactors == [plamenu2_user.acct], reaction
        # Phanpy and friends read the top-level mirror, not the nested one.
        assert (
            plamenu_api.get_status(posted["id"])["emoji_reactions"]
            == (plamenu_api.get_status(posted["id"])["pleroma"]["emoji_reactions"])
        )
        notification = wait_for(
            lambda: plamenu_api.notifications_from(
                plamenu2_user.acct, "pleroma:emoji_reaction"
            ),
            desc="a pleroma:emoji_reaction notification",
        )
        assert notification[0]["emoji"] == "🔥", notification[0]

    with step("undoing the reaction removes it"):
        plamenu2_api.unreact(theirs["id"], "🔥")
        wait_for(
            lambda: not plamenu_api.emoji_reactions(posted["id"]),
            desc="the Undo(EmojiReact) to clear the reaction",
        )

    with step("the reverse direction: our reaction to the peer's post"):
        their_post = plamenu2_api.post_status(f"react back {marker}")
        ours = interop.delivered(plamenu_api, f"react back {marker}")
        plamenu_api.react(ours["id"], "🎉")
        wait_for(
            lambda: any(
                r["name"] == "🎉" and r["count"] == 1
                for r in plamenu2_api.emoji_reactions(their_post["id"])
            ),
            desc="our EmojiReact to reach the peer",
        )
        plamenu_api.unreact(ours["id"], "🎉")
        wait_for(
            lambda: not plamenu2_api.emoji_reactions(their_post["id"]),
            desc="our Undo(EmojiReact) to reach the peer",
        )


@pytest.mark.federation(direction="both")
def test_edits_and_deletes_propagate(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """`Update(Note)` and `Delete(Note)` on a post the other side holds.

    Covers: the edit reaching the reader with `edited_at` set and the old text
    gone, the edit history the reader can page (both revisions, oldest first),
    the update notification a booster gets (Mastodon's rule: boosters are told
    the post they passed on has changed), and the delete tombstoning the
    reader's copy — in both directions."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user posts; the peer receives and boosts it"):
        posted = plamenu_api.post_status(f"first draft {marker}")
        theirs = interop.delivered(plamenu2_api, f"first draft {marker}")
        assert theirs["edited_at"] is None, theirs
        plamenu2_api.reblog(theirs["id"])

    with step("the author edits; the peer's copy is rewritten in place"):
        plamenu_api.edit_status(posted["id"], f"second draft {marker}")
        wait_for(
            lambda: "second draft" in plamenu2_api.get_status(theirs["id"])["content"],
            desc="the Update(Note) to rewrite the peer's copy",
        )
        updated = plamenu2_api.get_status(theirs["id"])
        assert "first draft" not in updated["content"], updated["content"]
        assert updated["edited_at"], "the reader must record the edit timestamp"
        assert updated["id"] == theirs["id"], "an edit must not create a new status"

    with step("the peer can page the edit history it was told about"):
        history = wait_for(
            lambda: (
                revisions
                if len(
                    revisions := plamenu2_api.get(
                        f"/api/v1/statuses/{theirs['id']}/history"
                    )
                )
                >= 2
                else None
            ),
            desc="both revisions to be listed in the peer's edit history",
        )
        assert "first draft" in history[0]["content"], history[0]
        assert "second draft" in history[-1]["content"], history[-1]

    with step("the booster is notified that what it passed on has changed"):
        wait_for(
            lambda: plamenu2_api.notifications_from(plamenu_user.acct, "update"),
            desc="an update notification for the peer that boosted it",
        )

    with step("the author deletes it; the peer's copy goes away"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: plamenu2_api.get_status_or_none(theirs["id"]) is None,
            desc="the Delete(Note) to remove the peer's copy",
        )

    with step("the reverse direction: the peer edits and deletes its own post"):
        their_post = plamenu2_api.post_status(f"peer draft {marker}")
        ours = interop.delivered(plamenu_api, f"peer draft {marker}")
        plamenu2_api.edit_status(their_post["id"], f"peer final {marker}")
        wait_for(
            lambda: "peer final" in plamenu_api.get_status(ours["id"])["content"],
            desc="the peer's Update(Note) to reach us",
        )
        plamenu2_api.delete_status(their_post["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(ours["id"]) is None,
            desc="the peer's Delete(Note) to reach us",
        )


@pytest.mark.federation(direction="both")
def test_every_visibility_survives_the_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """The four scopes mean the same thing on both sides of the wire.

    Covers: `public`/`unlisted`/`private`/`direct` addressing read back as the
    same scope by the receiver, the timeline rules that distinguish them
    (unlisted and below stay off the federated timeline, public does not), and
    the content warning + sensitive flag riding along."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("a post in each scope goes out to the follower"):
        sent = {}
        for scope in ("public", "unlisted", "private", "direct"):
            text = f"{scope} scope {marker}"
            if scope == "direct":
                text = f"@{plamenu2_user.acct} {text}"
            sent[scope] = plamenu_api.post_status(text, visibility=scope)
            assert sent[scope]["visibility"] == scope, sent[scope]

    with step("the reader reads every one of them back as the same scope"):
        for scope in ("public", "unlisted", "private"):
            received = wait_for(
                lambda scope=scope: plamenu2_api.home_status_containing(
                    f"{scope} scope {marker}"
                ),
                desc=f"the {scope} post to reach the follower's home timeline",
            )
            assert received["visibility"] == scope, received
            log(f"{scope} -> {received['visibility']}")
        # A direct message is deliberately not a home-timeline post on either
        # side of the wire; it is a conversation (and a mention).
        conversation = wait_for(
            lambda: plamenu2_api.conversation_containing(f"direct scope {marker}"),
            desc="the direct post to reach the follower as a conversation",
        )
        assert conversation["last_status"]["visibility"] == "direct", conversation
        assert conversation["unread"] is True, conversation
        assert plamenu2_api.home_status_containing(f"direct scope {marker}") is None, (
            "a direct message must not land on the home timeline"
        )

    with step("only the public post reaches the peer's federated timeline"):
        listed = [
            s["uri"]
            for s in plamenu2_api.public_timeline(limit=40)
            if marker in s["content"]
        ]
        assert listed == [sent["public"]["uri"]], listed

    with step("a content warning and the sensitive flag survive too"):
        cw = plamenu_api.post_status(
            f"behind the fold {marker}",
            spoiler_text=f"cw {marker}",
            sensitive="true",
        )
        received = interop.delivered(plamenu2_api, "behind the fold")
        assert received["spoiler_text"] == cw["spoiler_text"], received
        assert received["sensitive"] is True, received


@pytest.mark.federation(direction="both")
def test_poll_votes_federate(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A `Question` and the votes coming back to it.

    Covers: the poll options and flags surviving the wire, a remote vote
    reaching the author's tallies, the voter's own state on its own instance,
    the author's `poll` notification, and the same round trip the other way
    with a multiple-choice poll."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user posts a poll; the peer receives it whole"):
        posted = plamenu_api.post_poll(
            f"which one {marker}", ["fish", "fowl", "neither"]
        )
        theirs = interop.delivered(plamenu2_api, marker)
        poll = theirs["poll"]
        assert [o["title"] for o in poll["options"]] == ["fish", "fowl", "neither"]
        assert poll["multiple"] is False, poll
        assert poll["voted"] is False, poll
        assert poll["expires_at"], "the reader must know when voting closes"

    with step("the peer votes; the tally reaches the author"):
        voted = plamenu2_api.vote(poll["id"], [1])
        assert voted["own_votes"] == [1], voted
        wait_for(
            lambda: plamenu_api.get_status(posted["id"])["poll"]["votes_count"] == 1,
            desc="the remote vote to be counted by the poll's author",
        )
        tallies = plamenu_api.get_status(posted["id"])["poll"]
        assert [o["votes_count"] for o in tallies["options"]] == [0, 1, 0], tallies
        assert tallies["voters_count"] == 1, tallies

    with step("the voter's own instance remembers the vote"):
        assert plamenu2_api.get_poll(poll["id"])["voted"] is True

    with step("the reverse direction: a multiple-choice poll on the peer"):
        their_poll = plamenu2_api.post_poll(
            f"pick many {marker}", ["a", "b", "c"], multiple=True
        )
        ours = interop.delivered(plamenu_api, f"pick many {marker}")
        assert ours["poll"]["multiple"] is True, ours["poll"]
        plamenu_api.vote(ours["poll"]["id"], [0, 2])
        wait_for(
            lambda: (
                [
                    o["votes_count"]
                    for o in plamenu2_api.get_status(their_poll["id"])["poll"][
                        "options"
                    ]
                ]
                == [1, 0, 1]
            ),
            desc="both of our choices to be counted by the peer",
        )


@pytest.mark.federation(direction="both")
def test_the_status_a_reader_gets_matches_the_one_published(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """The whole entity, not one field of it, survives the round trip.

    A differential: the author's own rendering of a post against the reader's,
    with only the fields that are *supposed* to differ excluded. This is the
    check that catches a field quietly dropped from the wire — the kind that
    every foreign-peer test misses, because a foreign peer has no such field to
    lose."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("publish a post using as much of the wire as one Note can carry"):
        posted = plamenu_api.post_status(
            f"#{marker} hello @{plamenu2_user.acct}, a rich one",
            spoiler_text="mind the gap",
            sensitive="true",
            language="en",
            visibility="public",
        )

    with step("the reader's copy carries the same content, tags and mentions"):
        theirs = interop.delivered(plamenu2_api, marker)
        assert as_a_reader_rewrites_it(theirs["content"]) == as_a_reader_rewrites_it(
            posted["content"]
        ), (posted["content"], theirs["content"])
        assert theirs["spoiler_text"] == posted["spoiler_text"]
        assert theirs["sensitive"] == posted["sensitive"]
        assert theirs["language"] == posted["language"]
        assert theirs["visibility"] == posted["visibility"]
        assert theirs["created_at"] == posted["created_at"]
        assert [t["name"] for t in theirs["tags"]] == [marker], theirs["tags"]
        # The mentioned account is the reader's *own* user, so its handle is
        # bare there and fully qualified on the author's side — the same
        # account, named the way each instance names it.
        assert [m["acct"] for m in posted["mentions"]] == [plamenu2_user.acct], posted[
            "mentions"
        ]
        assert [m["acct"] for m in theirs["mentions"]] == [plamenu2_user.username], (
            theirs["mentions"]
        )
        # The tag link is the *reader's* own route: a hashtag means "search my
        # instance", not "visit the author's".
        assert theirs["tags"][0]["url"].startswith(plamenu2.url), theirs["tags"][0]
        # The mention, in contrast, points at the mentioned account's home.
        assert theirs["mentions"][0]["url"] == (
            f"{plamenu2.url}/@{plamenu2_user.username}"
        ), theirs["mentions"][0]

    with step("and the entity has the same shape on both sides"):
        author_side = shapes.skeleton(posted)
        reader_side = shapes.skeleton(theirs)
        # Four keys are the author's alone, by Mastodon's rules: `pinned` and
        # `application` exist only on your own statuses, and `roles`/`noindex`
        # only on accounts your instance hosts. A reader that grew them would
        # be inventing facts about somebody else's server.
        for key in ("pinned", "application"):
            assert key not in reader_side, f"{key} is not the reader's to report"
            author_side.pop(key)
        for key in ("roles", "noindex"):
            assert key not in reader_side["account"], (
                f"account.{key} is not the reader's to report"
            )
            author_side["account"].pop(key)
        divergences = shapes.diff(
            author_side, reader_side, reference="the author's own rendering"
        )
        assert not divergences, "the reader's status entity diverges:\n  - " + (
            "\n  - ".join(divergences)
        )
