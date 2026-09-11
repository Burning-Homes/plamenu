"""Federation between Plamenu and a real Lemmy instance.

Lemmy is the reference FEP-1b12 group host, so this exercises the
consumption paths against the real dialect (wrapped Announce + the compat
double-send, titled Page posts, origin-refetch trust) — and the reverse
direction: Plamenu interactions (votes, replies) landing in a Lemmy community,
and the outbound path, a Plamenu user originating a top-level thread into a community it
subscribes to (the Page round-trips back as group content). The peer is
optional: tests skip when it's down.
"""

import pytest
from plamenu_e2e import config, unique
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.lemmy import FRANK_NICK, LemmyApi, LemmyError
from plamenu_e2e.media import make_avif
from plamenu_e2e.steps import step, wait_for


def _follow_community(plamenu_api: Api, name: str) -> dict:
    """Resolve a Lemmy community as a Group account and follow it."""
    acct = f"{name}@{config.LEMMY_DOMAIN}"
    remote = plamenu_api.resolve_account(acct)
    assert remote, f"Plamenu cannot resolve {acct}"
    assert remote["group"] is True, remote
    plamenu_api.follow(remote["id"])
    wait_for(
        lambda: plamenu_api.relationship(remote["id"])["following"],
        desc="community Accept(Follow) to reach Plamenu",
    )
    return remote


@pytest.mark.federation(
    direction="inbound", reverse_of="test_lemmy_subscribes_to_plamenu_group"
)
def test_community_follow_and_wrapped_announce_lifecycle(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """Post → edit → comment → delete in a community Plamenu follows."""
    name = unique("plamlemmy")
    with step("frank creates a community on Lemmy"):
        community = lemmy_frank.create_community(name, "Plamenu e2e community")

    with step("Plamenu resolves the community (Group actor) and follows it"):
        group = _follow_community(plamenu_api, name)

    with step("frank's titled post arrives as a single boost by the community"):
        title = f"Announcing {marker}"
        posted = lemmy_frank.create_post(
            community["id"], title, body=f"post body {marker}"
        )
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="community-announced post on the Plamenu home timeline",
        )
        assert boost["account"]["group"] is True
        assert boost["account"]["acct"] == f"{name}@{config.LEMMY_DOMAIN}"
        # Lemmy's Page `name` is hoisted into the title column.
        assert boost["reblog"]["title"] == title
        # Lemmy double-sends (compat Announce(Page) + the FEP-1b12 wrapper);
        # dedup must leave exactly one boost of the post.
        uri = boost["reblog"]["uri"]
        wrappers = [
            s
            for s in plamenu_api.home_timeline(limit=40)
            if s.get("reblog") and s["reblog"]["uri"] == uri
        ]
        assert len(wrappers) == 1, [w["id"] for w in wrappers]

    with step("frank's edit reaches Plamenu via origin re-fetch"):
        edited = unique("lemmyedit")
        lemmy_frank.edit_post(posted["id"], body=f"post body {edited}")
        wait_for(
            lambda: edited in plamenu_api.get_status(boost["reblog"]["id"])["content"],
            desc="community-wrapped Update to reach Plamenu",
        )

    with step("frank's comment threads under the post without a second boost"):
        comment_marker = unique("lemmycomment")
        lemmy_frank.create_comment(posted["id"], f"comment {comment_marker}")
        wait_for(
            lambda: any(
                comment_marker in d["content"]
                for d in plamenu_api.context(boost["reblog"]["id"])["descendants"]
            ),
            desc="community-wrapped comment to thread under the post",
        )
        assert plamenu_api.home_status_containing(comment_marker) is None

    with step("frank's delete blanks the post on Plamenu"):
        lemmy_frank.delete_post(posted["id"])
        # frank's comment is still live beneath the post, so the deletion leaves
        # a "deleted status" placeholder rather than dropping the row — the same
        # rule that keeps a soft-deleted middle post threading. A post with no
        # live reply is removed outright (test_deletes.py covers that side).
        wait_for(
            lambda: (
                (plamenu_api.get_status_or_none(boost["reblog"]["id"]) or {}).get(
                    "deleted"
                )
                is True
            ),
            desc="community-wrapped Delete to blank the post on Plamenu",
        )
        placeholder = plamenu_api.get_status(boost["reblog"]["id"])
        assert edited not in placeholder["content"], placeholder["content"]
        assert placeholder["title"] is None

    with step("Plamenu leaves; frank removes the community"):
        plamenu_api.unfollow(group["id"])
        wait_for(
            lambda: not plamenu_api.relationship(group["id"])["following"],
            desc="Undo(Follow) of the community",
        )
        lemmy_frank.delete_community(community["id"])


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="Lemmy-specific inbound regression: multiple AVIF images are represented as inline HTML rather than structured attachments.",
)
def test_lemmy_post_with_two_inline_avifs_becomes_two_images(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """Regression for lemmy.zip-style posts whose two pict-rs AVIFs live in
    the Page body. They must be decoded, cached, and described as images — not
    exposed as two application/octet-stream `.bin` downloads."""
    name = unique("plamavif")
    with step("create and follow a Lemmy image community"):
        community = lemmy_frank.create_community(name, "Plamenu AVIF e2e")
        group = _follow_community(plamenu_api, name)

    with step("upload two real AVIFs and publish both inline"):
        first = lemmy_frank.upload_image(make_avif(80, 48, (220, 40, 30)))
        second = lemmy_frank.upload_image(make_avif(45, 75, (30, 80, 220)))
        body = (
            f"two AVIF images {marker}\n\n"
            f"![wide red image]({first['url']})\n\n"
            f"![tall blue image]({second['url']})"
        )
        lemmy_frank.create_post(community["id"], f"AVIF pair {marker}", body=body)

    with step("Plamenu recovers and caches both images"):
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="the Lemmy AVIF post to reach Plamenu",
        )

        def cached_pair():
            attachments = plamenu_api.get_status(boost["reblog"]["id"]).get(
                "media_attachments", []
            )
            if len(attachments) != 2:
                return None
            if not all(
                attachment["type"] == "image"
                and attachment["url"].startswith(f"{config.PLAMENU_URL}/media/")
                and attachment["url"].endswith(".avif")
                and attachment.get("blurhash")
                and attachment.get("meta", {}).get("original", {}).get("width")
                for attachment in attachments
            ):
                return None
            return attachments

        attachments = wait_for(
            cached_pair,
            desc="both AVIF attachments to be decoded and cached as images",
        )
        assert [
            (a["meta"]["original"]["width"], a["meta"]["original"]["height"])
            for a in attachments
        ] == [
            (80, 48),
            (45, 75),
        ]

    with step("both cached files serve as AVIF rather than binary downloads"):
        for attachment in attachments:
            response = plamenu_api.http.get(attachment["url"], timeout=30)
            assert response.ok
            assert response.headers["content-type"].startswith("image/avif")
            assert b"ftypavif" in response.content[:64]

    with step("leave and remove the disposable community"):
        plamenu_api.unfollow(group["id"])
        lemmy_frank.delete_community(community["id"])


@pytest.mark.federation(
    direction="outbound",
    reverse_of="test_community_follow_and_wrapped_announce_lifecycle",
)
def test_lemmy_subscribes_to_plamenu_group(
    lemmy_frank: LemmyApi, plamenu_user, plamenu_api: Api, cli
):
    """Lemmy resolves a Plamenu-hosted group as a community and
    subscribes — open groups accept instantly, approval groups hold the
    request as Pending."""
    open_name = unique("plamhost")
    held_name = unique("plamheld")
    with step("two local groups are created on Plamenu (open + approval)"):
        cli.group_add(open_name, owner=plamenu_user.username)
        cli.group_add(held_name, owner=plamenu_user.username, approval=True)

    with step("Lemmy resolves the open group as a community"):
        resolved = lemmy_frank.resolve(f"!{open_name}@{config.PLAMENU_DOMAIN}")
        community = resolved["community"]["community"]
        assert community["name"] == open_name
        assert community["local"] is False

    with step("frank subscribes; the group auto-accepts"):
        lemmy_frank.follow_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["subscribed"]
                == "Subscribed"
            ),
            desc="the group's Accept(Follow) to reach Lemmy",
        )

    with step("frank shows up in the group's membership on Plamenu"):
        # The owner is a member from creation, so frank makes two.
        wait_for(
            lambda: (plamenu_api.lookup(open_name) or {}).get("followers_count") == 2,
            desc="the follower to land in the group's followers collection",
        )

    with step("an approval-mode group holds frank's request as Pending"):
        held = lemmy_frank.resolve(f"!{held_name}@{config.PLAMENU_DOMAIN}")
        held_community = held["community"]["community"]
        lemmy_frank.follow_community(held_community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(held_community["id"])["subscribed"]
                == "Pending"
            ),
            desc="the join request to register as pending on Lemmy",
        )

    with step("cleanup: frank unsubscribes from the open group"):
        lemmy_frank.follow_community(community["id"], follow=False)
        wait_for(
            lambda: (plamenu_api.lookup(open_name) or {}).get("followers_count") == 1,
            desc="the Undo(Follow) to drop the membership",
        )


@pytest.mark.federation(direction="both")
def test_lemmy_consumes_plamenu_hosted_group(
    lemmy_frank: LemmyApi, plamenu_user, plamenu_api: Api, cli, marker: str
):
    """The hosted-group lifecycle against real Lemmy. Plamenu wraps
    member submissions in the group's Announce (verbatim, plus the compat
    double-send) and Lemmy renders them as community threads; Lemmy members
    comment and open threads back through the group's inbox."""
    name = unique("plamforum")
    with step("a local group exists and frank subscribes from Lemmy"):
        cli.group_add(name, owner=plamenu_user.username)
        resolved = lemmy_frank.resolve(f"!{name}@{config.PLAMENU_DOMAIN}")
        community = resolved["community"]["community"]
        lemmy_frank.follow_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["subscribed"]
                == "Subscribed"
            ),
            desc="the group's Accept(Follow) to reach Lemmy",
        )
    group_id = plamenu_api.lookup(name)["id"]

    with step("a titled Plamenu submission becomes a Lemmy thread"):
        title = f"Thread {marker}"
        status = plamenu_api.post_status(
            f"forum body {marker}", title=title, group_id=group_id
        )
        assert status["title"] == title
        thread = wait_for(
            lambda: next(
                (
                    p
                    for p in lemmy_frank.posts_of(community["id"])
                    if p["post"]["name"] == title
                ),
                None,
            ),
            desc="the group's Announce(Create(Page)) to land as a Lemmy post",
        )
        assert marker in (thread["post"].get("body") or "")

    with step("the author's edit updates the Lemmy thread"):
        edited = unique("forumedit")
        plamenu_api.edit_status(status["id"], f"forum body {edited}")
        wait_for(
            lambda: (
                edited
                in (
                    lemmy_frank.post_view(thread["post"]["id"])["post"].get("body")
                    or ""
                )
            ),
            desc="the group-wrapped Update to reach Lemmy",
        )

    with step("frank's Lemmy comment threads back under the Plamenu post"):
        comment_marker = unique("frankcomment")
        lemmy_frank.create_comment(thread["post"]["id"], f"from lemmy {comment_marker}")
        wait_for(
            lambda: any(
                comment_marker in d["content"]
                for d in plamenu_api.context(status["id"])["descendants"]
            ),
            desc="the comment to arrive at the group inbox and thread under the post",
        )

    with step("frank opens a thread in the Plamenu-hosted community"):
        frank_marker = unique("frankpost")
        frank_title = f"From Lemmy {frank_marker}"
        lemmy_frank.create_post(
            community["id"], frank_title, body=f"hello from lemmy {frank_marker}"
        )
        # The group's boost of frank's submission puts it into member (here:
        # the owner's) home timelines, title hoisted into the title column.
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(frank_marker),
            desc="the group to announce frank's submission into member timelines",
        )
        assert boost["account"]["acct"] == name
        assert boost["account"]["group"] is True
        assert boost["reblog"]["title"] == frank_title

    with step("the author's delete removes the Lemmy thread"):
        plamenu_api.delete_status(status["id"])
        wait_for(
            lambda: lemmy_frank.post_view(thread["post"]["id"])["post"]["deleted"],
            desc="the group-wrapped Delete to reach Lemmy",
        )

    with step("cleanup: frank unsubscribes"):
        lemmy_frank.follow_community(community["id"], follow=False)


@pytest.mark.federation(
    direction="outbound", reverse_of="test_lemmy_votes_on_plamenu_hosted_group"
)
def test_plamenu_interactions_reach_lemmy(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """A Plamenu favourite counts as an upvote and a reply as a comment."""
    name = unique("plamvotes")
    with step("frank creates a community"):
        community = lemmy_frank.create_community(name, "Plamenu votes e2e")

    with step("Plamenu follows the community, then frank posts"):
        # Follow first: Lemmy only Announces to followers that exist at
        # post time, so a pre-existing post never reaches a late follower.
        _follow_community(plamenu_api, name)
        posted = lemmy_frank.create_post(
            community["id"], f"Vote target {marker}", body=f"vote body {marker}"
        )
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="community post on the Plamenu home timeline",
        )
        status_id = boost["reblog"]["id"]

    with step("a Plamenu favourite arrives as an upvote"):
        base_score = lemmy_frank.post_view(posted["id"])["counts"]["score"]
        plamenu_api.favourite(status_id)
        wait_for(
            lambda: (
                lemmy_frank.post_view(posted["id"])["counts"]["score"] == base_score + 1
            ),
            desc="Like to count as a Lemmy upvote",
        )

    with step("a Plamenu downvote flips the Lemmy score"):
        # The downvote retracts the upvote (Undo(Like)) and sends a Dislike
        # to the community: +1 becomes -1.
        plamenu_api.downvote(status_id)
        wait_for(
            lambda: (
                lemmy_frank.post_view(posted["id"])["counts"]["score"] == base_score - 1
            ),
            desc="Dislike and the retracted Like to flip the Lemmy score",
        )

    with step("retracting the downvote returns the score to baseline"):
        plamenu_api.undownvote(status_id)
        wait_for(
            lambda: (
                lemmy_frank.post_view(posted["id"])["counts"]["score"] == base_score
            ),
            desc="Undo(Dislike) to clear the Lemmy vote",
        )

    with step("a Plamenu reply arrives as a comment"):
        reply_marker = unique("plamreply")
        plamenu_api.post_status(
            f"reply from plamenu {reply_marker}", in_reply_to_id=status_id
        )
        wait_for(
            lambda: any(
                reply_marker in c["comment"]["content"]
                for c in lemmy_frank.comments_of(posted["id"])
            ),
            desc="reply to appear as a Lemmy comment",
        )

    with step("cleanup: frank removes the community"):
        lemmy_frank.delete_community(community["id"])


def _community_post_by_title(lemmy: LemmyApi, community_id: int, needle: str):
    """The community's post whose title contains `needle`, or None."""
    return next(
        (
            p
            for p in lemmy.posts_of(community_id)
            if needle in p["post"].get("name", "")
        ),
        None,
    )


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound-only: a Plamenu user originates a top-level post into a remote Lemmy community; no reciprocal inbound test distinct from the group lifecycle.",
)
def test_plamenu_originates_post_into_lemmy_community(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """A Plamenu user posts a top-level thread *into* a Lemmy community it
    subscribes to. The Page reaches Lemmy as a community post, comes back
    announced as group content on Plamenu (attributed without duplicating the
    author's own copy — the returning Announce is short-circuited for our own
    activity), and the author's edit and delete propagate to Lemmy."""
    name = unique("plamorig")
    with step("frank creates a community and Plamenu subscribes"):
        community = lemmy_frank.create_community(name, "Plamenu originates e2e")
        group = _follow_community(plamenu_api, name)

    with step("Plamenu posts a titled thread into the community"):
        title = f"Plamenu thread {marker}"
        posted = plamenu_api.post_status(
            f"body from plamenu {marker}", group_id=group["id"], title=title
        )
        post_view = wait_for(
            lambda: _community_post_by_title(lemmy_frank, community["id"], marker),
            desc="the Plamenu post to appear in the Lemmy community",
        )
        assert post_view["post"]["name"] == title
        lemmy_post_id = post_view["post"]["id"]

    with step("the community announces it back as group content on Plamenu"):
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="the community's Announce of our own post",
        )
        assert boost["account"]["group"] is True
        assert boost["account"]["acct"] == f"{name}@{config.LEMMY_DOMAIN}"
        # The boost points at our own original post — not a re-ingested copy.
        assert boost["reblog"]["id"] == posted["id"]

    with step("a Plamenu edit reaches Lemmy"):
        edited = unique("plamedit")
        plamenu_api.edit_status(posted["id"], f"edited from plamenu {edited}")
        wait_for(
            lambda: (
                edited in lemmy_frank.post_view(lemmy_post_id)["post"].get("body", "")
            ),
            desc="the edit to reach the Lemmy post body",
        )

    with step("a Plamenu delete removes the post from Lemmy"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: (
                _community_post_by_title(lemmy_frank, community["id"], marker) is None
            ),
            desc="the delete to remove the post from the Lemmy community",
        )

    with step("cleanup: frank removes the community"):
        lemmy_frank.delete_community(community["id"])


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound-only refinement of test_plamenu_originates_post_into_lemmy_community: posting into a remote community works without first subscribing; no inbound counterpart.",
)
def test_plamenu_posts_to_a_community_without_following(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """Following is NOT required to post into a remote community — we don't
    own its policy, so we defer to it and the remote enforces. The post reaches
    Lemmy even with no prior subscription (it just won't come back attributed)."""
    name = unique("plamnofollow")
    with step("frank creates a community; Plamenu resolves it but does NOT follow"):
        community = lemmy_frank.create_community(name, "Plamenu no-follow e2e")
        group = plamenu_api.resolve_account(f"{name}@{config.LEMMY_DOMAIN}")
        assert group and group["group"] is True
        assert plamenu_api.relationship(group["id"])["following"] is False

    with step("Plamenu posts into the community with no subscription"):
        title = f"No-follow thread {marker}"
        plamenu_api.post_status(
            f"body from a non-subscriber {marker}", group_id=group["id"], title=title
        )
        post_view = wait_for(
            lambda: _community_post_by_title(lemmy_frank, community["id"], marker),
            desc="the post to reach Lemmy without a prior follow",
        )
        assert post_view["post"]["name"] == title

    with step("cleanup: frank removes the community"):
        lemmy_frank.delete_community(community["id"])


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_interactions_reach_lemmy"
)
def test_lemmy_votes_on_plamenu_hosted_group(
    lemmy_frank: LemmyApi, plamenu_user, plamenu_api: Api, cli, marker: str
):
    """Inbound: a Lemmy member up/downvotes a post in a Plamenu-hosted
    community. The vote reaches the group inbox and the group is authoritative
    for it, so the hosted post's score reflects it."""
    name = unique("plamscore")
    with step("a local group exists and frank subscribes from Lemmy"):
        cli.group_add(name, owner=plamenu_user.username)
        resolved = lemmy_frank.resolve(f"!{name}@{config.PLAMENU_DOMAIN}")
        community = resolved["community"]["community"]
        lemmy_frank.follow_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["subscribed"]
                == "Subscribed"
            ),
            desc="the group's Accept(Follow) to reach Lemmy",
        )
    group_id = plamenu_api.lookup(name)["id"]

    with step("a titled Plamenu submission becomes a Lemmy thread"):
        title = f"Score thread {marker}"
        status = plamenu_api.post_status(
            f"score body {marker}", title=title, group_id=group_id
        )
        assert status["group_post"] is True, status
        thread = wait_for(
            lambda: next(
                (
                    p
                    for p in lemmy_frank.posts_of(community["id"])
                    if p["post"]["name"] == title
                ),
                None,
            ),
            desc="the group's Announce(Create(Page)) to land as a Lemmy post",
        )
        post_id = thread["post"]["id"]

    with step("frank's Lemmy upvote lands as a favourite on the hosted post"):
        lemmy_frank.vote_post(post_id, 1)
        wait_for(
            lambda: plamenu_api.get_status(status["id"])["favourites_count"] == 1,
            desc="the community-addressed Like to reach the group and score the post",
        )

    with step("frank's Lemmy downvote displaces the upvote"):
        lemmy_frank.vote_post(post_id, -1)
        wait_for(
            lambda: (
                plamenu_api.get_status(status["id"])["downvotes_count"] == 1
                and plamenu_api.get_status(status["id"])["favourites_count"] == 0
            ),
            desc="the Dislike to store a downvote and retract the upvote",
        )

    with step("frank clears the vote"):
        lemmy_frank.vote_post(post_id, 0)
        wait_for(
            lambda: plamenu_api.get_status(status["id"])["downvotes_count"] == 0,
            desc="the Undo(Dislike) to clear the downvote",
        )

    with step("cleanup: frank unsubscribes and the author deletes the thread"):
        plamenu_api.delete_status(status["id"])
        lemmy_frank.follow_community(community["id"], follow=False)


@pytest.mark.federation(
    direction="outbound", reverse_of="test_lemmy_mod_actions_reach_plamenu"
)
def test_plamenu_mod_actions_reach_lemmy(
    lemmy_frank: LemmyApi, plamenu_user, plamenu_api: Api, cli, marker: str
):
    """A Plamenu-hosted community's moderator actions reach Lemmy. A
    lock/unlock federates as `Lock`/`Undo(Lock)` and a removal as a mod-`Delete`
    (a `Delete` carrying a reason), which Lemmy applies to its copy of the post
    (verifying the acting moderator against the community's mod list)."""
    name = unique("plammod")
    with step("a local group exists and frank subscribes from Lemmy"):
        cli.group_add(name, owner=plamenu_user.username)
        resolved = lemmy_frank.resolve(f"!{name}@{config.PLAMENU_DOMAIN}")
        community = resolved["community"]["community"]
        lemmy_frank.follow_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["subscribed"]
                == "Subscribed"
            ),
            desc="the group's Accept(Follow) to reach Lemmy",
        )
    group_id = plamenu_api.lookup(name)["id"]

    with step("the owner's titled post becomes a Lemmy thread"):
        title = f"Mod thread {marker}"
        status = plamenu_api.post_status(
            f"mod body {marker}", title=title, group_id=group_id
        )
        thread = wait_for(
            lambda: next(
                (
                    p
                    for p in lemmy_frank.posts_of(community["id"])
                    if p["post"]["name"] == title
                ),
                None,
            ),
            desc="the group's Announce(Create(Page)) to land as a Lemmy post",
        )
        post_id = thread["post"]["id"]

    with step("locking the thread reaches Lemmy"):
        cli.group_lock(name, status["id"])
        wait_for(
            lambda: lemmy_frank.post_view(post_id)["post"]["locked"] is True,
            desc="the group's Lock to lock the Lemmy post",
        )

    with step("unlocking reopens it on Lemmy"):
        cli.group_lock(name, status["id"], unlock=True)
        wait_for(
            lambda: lemmy_frank.post_view(post_id)["post"]["locked"] is False,
            desc="the group's Undo(Lock) to reopen the Lemmy post",
        )

    with step("removing the post reaches Lemmy as a mod removal"):
        cli.group_remove(name, status["id"])
        wait_for(
            lambda: lemmy_frank.post_view(post_id)["post"]["removed"] is True,
            desc="the group's mod-Delete to remove the Lemmy post",
        )
        # The local status itself survives a group removal.
        assert plamenu_api.get_status(status["id"])["id"] == status["id"]

    with step("cleanup: frank unsubscribes"):
        lemmy_frank.follow_community(community["id"], follow=False)


@pytest.mark.federation(
    direction="outbound", reverse_of="test_lemmy_community_lifecycle_reaches_plamenu"
)
def test_plamenu_group_lifecycle_reaches_lemmy(
    lemmy_frank: LemmyApi, plamenu_user, cli, marker: str
):
    """A Plamenu-hosted community's profile Update and Delete reach
    Lemmy. A rename federates as the owner-authored Update(Group) wrapped in the
    group's Announce (Lemmy's UpdateCommunity), and a group deletion federates
    Delete(Group). Lemmy accepts both because the acting owner shares the
    community's domain (its verify_mod_action treats a same-instance actor as
    staff)."""
    name = unique("plamlife")
    with step("a local group exists and frank subscribes from Lemmy"):
        cli.group_add(name, owner=plamenu_user.username, display_name="Before")
        resolved = lemmy_frank.resolve(f"!{name}@{config.PLAMENU_DOMAIN}")
        community = resolved["community"]["community"]
        assert community["title"] == "Before"
        lemmy_frank.follow_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["subscribed"]
                == "Subscribed"
            ),
            desc="the group's Accept(Follow) to reach Lemmy",
        )

    with step("renaming the group refreshes the Lemmy community"):
        new_title = f"Renamed {marker}"
        cli.group_rename(name, display_name=new_title)
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["community"]["title"]
                == new_title
            ),
            desc="the owner-authored Update(Group) to refresh the Lemmy community",
        )

    with step("deleting the group marks the Lemmy community deleted"):
        cli.group_delete(name)
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["community"]["deleted"]
                is True
            ),
            desc="the Delete(Group) to mark the Lemmy community deleted",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound discovery: Lemmy serves a user and a community under one handle; Plamenu disambiguates Person vs Group on resolve. A local resolution behavior with no outbound counterpart.",
)
def test_user_and_community_same_handle_disambiguate(
    lemmy_frank: LemmyApi, plamenu_api: Api
):
    """Lemmy serves a user and a community under one name (separate
    namespaces), so `acct:frank@lemmy.local` webfingers as BOTH a Person
    (`/u/frank`) and a Group (`/c/frank`). Plamenu must keep them distinct:
    `@` resolves the person, `!` the community, a bare handle both — none of
    which the old single-slot `(username, domain)` model could represent."""
    # `frank` is the standing Lemmy admin *user*; give the same name to a
    # community so the handle collides. Idempotent across reruns.
    with step("a community named `frank` exists alongside the user"):
        try:
            lemmy_frank.create_community(FRANK_NICK, "Frank's community")
        except LemmyError:
            pass  # already created by an earlier run — fine
    acct = f"{FRANK_NICK}@{config.LEMMY_DOMAIN}"

    with step("`@frank@host` resolves the Person, not the Group"):
        person = plamenu_api.resolve_account(acct)  # resolve_account prefixes `@`
        assert person, f"@{acct} did not resolve"
        assert person["group"] is False, person
        assert person["acct"] == acct
        assert "/u/" in person["uri"], person["uri"]

    with step("`!frank@host` resolves the Group"):
        groups = plamenu_api.search(f"!{acct}", resolve=True, type="accounts")[
            "accounts"
        ]
        assert groups, f"!{acct} did not resolve"
        group = groups[0]
        assert group["group"] is True, group
        assert group["acct"] == acct
        assert "/c/" in group["uri"], group["uri"]

    with step("they are genuinely distinct actors"):
        assert person["id"] != group["id"]
        assert person["uri"] != group["uri"]

    with step("a bare handle surfaces both actors"):
        both = plamenu_api.search(acct, resolve=True, type="accounts")["accounts"]
        uris = {a["uri"] for a in both}
        assert person["uri"] in uris, both
        assert group["uri"] in uris, both


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_mod_actions_reach_lemmy"
)
def test_lemmy_mod_actions_reach_plamenu(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """Reverse of test_plamenu_mod_actions_reach_lemmy: a Lemmy community's
    moderator actions reaching Plamenu, exercised through the wrapped Announce
    (inbound community-lifecycle reflection).

    All three reflect on the Plamenu side now:
    - Lock / Undo(Lock): the wrapped Lock is stored against the (remote) group
      account and the thread root, so `group_locked` flips on the Status API and
      a reply into the locked thread is refused up front (not silently
      un-delivered).
    - mod removal: a mod-`Delete` (frank == author in this single-owner stack)
      reaches the forwarded-Delete path; Lemmy serves the removed post 410, so
      `origin_confirms_gone` drops the boosted copy.
    - Undo(Delete) restore: the wrapped Undo(Delete) re-fetches the now-live
      origin Page and re-records the community boost (a fresh status id)."""
    name = unique("lemmymod")
    with step("frank creates a community and Plamenu follows it"):
        community = lemmy_frank.create_community(name, "Mod actions e2e")
        group = _follow_community(plamenu_api, name)

    with step("frank posts a titled thread; Plamenu records the community boost"):
        posted = lemmy_frank.create_post(
            community["id"], f"Mod thread {marker}", body=f"mod body {marker}"
        )
        boost = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="community-announced post on the Plamenu home timeline",
        )
        status_id = boost["reblog"]["id"]

    with step("frank locks the thread; Plamenu flips group_locked and refuses replies"):
        lemmy_frank.lock_post(posted["id"], True)
        wait_for(
            lambda: lemmy_frank.post_view(posted["id"])["post"]["locked"] is True,
            desc="Lemmy to record the thread lock",
        )
        wait_for(
            lambda: plamenu_api.get_status(status_id)["group_locked"] is True,
            desc="the wrapped Lock to surface as group_locked on Plamenu",
        )
        with pytest.raises(ApiError):
            plamenu_api.post_status(f"late reply {marker}", in_reply_to_id=status_id)

    with step("frank unlocks the thread; Plamenu clears group_locked"):
        lemmy_frank.lock_post(posted["id"], False)
        wait_for(
            lambda: lemmy_frank.post_view(posted["id"])["post"]["locked"] is False,
            desc="Lemmy to clear the thread lock",
        )
        wait_for(
            lambda: plamenu_api.get_status(status_id)["group_locked"] is False,
            desc="the wrapped Undo(Lock) to clear group_locked on Plamenu",
        )

    with step("frank removes the post as a mod action; Plamenu drops the copy"):
        lemmy_frank.remove_post(posted["id"], removed=True, reason=f"e2e {marker}")
        wait_for(
            lambda: plamenu_api.get_status_or_none(status_id) is None,
            desc="the community's mod-Delete to drop the post from Plamenu",
        )

    with step("frank restores the post; Plamenu re-ingests it and the community boost"):
        lemmy_frank.remove_post(posted["id"], removed=False)
        wait_for(
            lambda: lemmy_frank.post_view(posted["id"])["post"]["removed"] is False,
            desc="Lemmy to restore the post",
        )
        # The restore mints a fresh status id, so re-capture by content, not by
        # the (now 404) original id.
        restored = wait_for(
            lambda: plamenu_api.home_reblog_containing(marker),
            desc="the wrapped Undo(Delete) to re-ingest the restored community post",
        )
        assert marker in restored["reblog"]["content"], restored

    with step("cleanup: Plamenu leaves and frank removes the community"):
        plamenu_api.unfollow(group["id"])
        lemmy_frank.delete_community(community["id"])


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_group_lifecycle_reaches_lemmy"
)
def test_lemmy_community_lifecycle_reaches_plamenu(
    lemmy_frank: LemmyApi, plamenu_api: Api, marker: str
):
    """Reverse of test_plamenu_group_lifecycle_reaches_lemmy: a Lemmy community's
    rename and deletion reaching Plamenu (the delete reflection is covered).

    The rename is a genuine inbound reflection: Lemmy announces UpdateCommunity to
    followers, and Plamenu's handle_group_announce Update branch re-fetches and
    upserts the Group actor, refreshing `display_name` (mapped from the Group's
    `name`, which is Lemmy's community `title`).

    The community *delete* now reflects too: Lemmy announces
    Delete(actor=person, object=community); Plamenu gone-confirms against the
    origin and drops the stored remote group account (its boost rows cascade
    away), so the account 404s afterward."""
    name = unique("lemmylife")
    with step("frank creates a community titled 'Before' and Plamenu follows it"):
        community = lemmy_frank.create_community(name, "Before")
        group = _follow_community(plamenu_api, name)
        remote_id = group["id"]
        assert plamenu_api.account(remote_id)["display_name"] == "Before", (
            plamenu_api.account(remote_id)
        )

    with step("frank renames the community; Plamenu refreshes the stored group"):
        new_title = f"Renamed {marker}"
        lemmy_frank.edit_community(community["id"], title=new_title)
        wait_for(
            lambda: plamenu_api.account(remote_id)["display_name"] == new_title,
            desc="the community's Announce(Update(Group)) to refresh display_name",
        )

    with step("frank deletes the community; Plamenu drops the stored group account"):
        lemmy_frank.delete_community(community["id"])
        wait_for(
            lambda: (
                lemmy_frank.community_view(community["id"])["community"]["deleted"]
                is True
            ),
            desc="Lemmy to mark the community deleted",
        )
        wait_for(
            lambda: plamenu_api.account(remote_id) is None,
            desc="the wrapped Delete(Group) to drop the stored remote community",
        )
        # The deletion cascaded our follow away; no unfollow cleanup is needed.
