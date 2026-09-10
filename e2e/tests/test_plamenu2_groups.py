"""Plamenu against Plamenu: groups.

The group dialect is the one this server shares with Lemmy — a
`Group` actor that announces its members' submissions, votes that score them,
and moderator verbs (`Lock`, mod-`Delete`, `Block`) that travel with the
announce. Against Lemmy each of those is checked against Lemmy's reading of
it; here both the hosting side and the member side are this server, so the
member half of every one of those verbs — what a *reader* does with an
inbound Lock, a mod removal, a group rename — is exercised for the first
time.

Direction is `both` throughout: the two ends run identical software, so
splitting a round trip in two would assert the same code twice.
"""

import pytest
from plamenu_e2e import interop, unique
from plamenu_e2e.steps import log, step, wait_for


def group_post_by_title(api, group_id: str, title: str):
    """The group's post carrying `title`, as `api`'s instance lists it.

    A group's own timeline is its announces, so the submission is the boosted
    status inside the wrapper; a post the group itself authored would be the
    entry proper."""
    for entry in api.account_statuses(group_id):
        status = entry.get("reblog") or entry
        if title in (status.get("content") or ""):
            return status
    return None


@pytest.mark.federation(direction="both")
def test_remote_member_joins_and_posts_into_a_hosted_group(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """The whole life of a thread in a group hosted on the other server.

    Covers: resolving a `Group` actor by its `!name@host` handle, the join
    (a Follow the group auto-accepts) counting as membership, a remote
    member's titled submission reaching the group and coming back announced,
    a reply from the hosting side threading under it, and the author's edit
    and delete following the post wherever the announce took it."""
    name = unique("plam2forum")

    with step("a group is created on the local instance"):
        cli.group_add(name, owner=plamenu_user.username, display_name=f"Forum {marker}")
        group_local_id = plamenu_api.lookup(name)["id"]

    with step("the peer's user resolves it as a group and joins"):
        group = interop.resolve_group(plamenu2_api, f"{name}@{plamenu_user.domain}")
        assert group["group"] is True, group
        assert group["display_name"] == f"Forum {marker}", group
        plamenu2_api.follow(group["id"])
        wait_for(
            lambda: plamenu2_api.relationship(group["id"])["following"],
            desc="the group's Accept(Follow) to reach the joining member",
        )
        # The owner is a member from creation, so the joiner makes two.
        wait_for(
            lambda: plamenu_api.lookup(name)["followers_count"] == 2,
            desc="the remote member to land in the group's followers collection",
        )

    with step("the member submits a titled thread from its own instance"):
        title = f"Thread {marker}"
        submission = plamenu2_api.post_status(
            f"forum body {marker}", title=title, group_id=group["id"]
        )
        assert submission["title"] == title, submission
        # Not a group post *yet*: the submission only becomes group content
        # once the group has announced it, and the group is somebody else's.
        assert submission["group_post"] is False, submission

    with step("the group announces it, and the hosting side sees the thread"):
        hosted = wait_for(
            lambda: group_post_by_title(plamenu_api, group_local_id, title),
            desc="the submission to be announced by the group",
        )
        assert hosted["uri"] == submission["uri"], hosted
        assert hosted["account"]["acct"] == plamenu2_user.acct, hosted["account"]
        assert hosted["group_post"] is True, hosted
        assert marker in hosted["content"], hosted["content"]

    with step("the announce comes back to the member as group content"):
        boost = wait_for(
            lambda: plamenu2_api.home_reblog_containing(marker),
            desc="the group's Announce of the member's own post",
        )
        assert boost["account"]["group"] is True, boost["account"]
        assert boost["account"]["acct"] == f"{name}@{plamenu_user.domain}", boost[
            "account"
        ]
        assert boost["reblog"]["id"] == submission["id"], (
            "the announce must point at the member's own post, not a copy"
        )
        assert plamenu2_api.get_status(submission["id"])["group_post"] is True, (
            "once announced, the member's own post is group content"
        )

    with step("a reply from the hosting side reaches the member"):
        plamenu_api.post_status(
            f"@{plamenu2_user.acct} good thread {marker}c",
            in_reply_to_id=hosted["id"],
        )
        wait_for(
            lambda: any(
                f"{marker}c" in s["content"]
                for s in plamenu2_api.context(submission["id"])["descendants"]
            ),
            desc="the group-routed reply to reach the member's thread",
        )

    with step("the author's edit and delete follow the post"):
        plamenu2_api.edit_status(submission["id"], f"forum body {marker} (edited)")
        wait_for(
            lambda: "(edited)" in plamenu_api.get_status(hosted["id"])["content"],
            desc="the group-wrapped Update to reach the hosting side",
        )
        plamenu2_api.delete_status(submission["id"])
        # The thread has a reply by now, so the hosting side keeps a blanked
        # stub rather than tearing a hole in the conversation.
        wait_for(
            lambda: (
                marker
                not in (
                    (plamenu_api.get_status_or_none(hosted["id"]) or {}).get("content")
                    or ""
                )
            ),
            desc="the group-wrapped Delete to blank the hosting side's copy",
        )


@pytest.mark.federation(direction="both")
def test_group_votes_score_posts_on_both_sides(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """Up- and downvotes on a hosted thread, cast from the other instance.

    Covers: the community-addressed `Like` and `Dislike` reaching the group
    (which is authoritative for a hosted post's score), a downvote displacing
    an upvote, the `Undo` clearing it, and the resulting score being visible to
    the voter's own instance as well as the host's."""
    name = unique("plam2score")

    with step("a group exists locally and the peer's user joins it"):
        cli.group_add(name, owner=plamenu_user.username)
        group_local_id = plamenu_api.lookup(name)["id"]
        group = interop.resolve_group(plamenu2_api, f"{name}@{plamenu_user.domain}")
        plamenu2_api.follow(group["id"])
        wait_for(
            lambda: plamenu2_api.relationship(group["id"])["following"],
            desc="the group's Accept(Follow) to reach the joining member",
        )

    with step("the owner submits a thread the peer's user can see"):
        title = f"Score thread {marker}"
        submission = plamenu_api.post_status(
            f"score body {marker}", title=title, group_id=group_local_id
        )
        theirs = wait_for(
            lambda: plamenu2_api.resolve_status(submission["uri"]),
            desc="the announced thread to reach the member",
        )

    with step("an upvote from the member scores the hosted post"):
        plamenu2_api.favourite(theirs["id"])
        wait_for(
            lambda: plamenu_api.get_status(submission["id"])["favourites_count"] == 1,
            desc="the Like to reach the group and score the post",
        )

    with step("a downvote displaces it"):
        plamenu2_api.downvote(theirs["id"])
        wait_for(
            lambda: (
                plamenu_api.get_status(submission["id"])["downvotes_count"] == 1
                and plamenu_api.get_status(submission["id"])["favourites_count"] == 0
            ),
            desc="the Dislike to store a downvote and retract the upvote",
        )
        log("score on the host: -1")

    with step("and clearing the vote takes the score back to nothing"):
        plamenu2_api.undownvote(theirs["id"])
        wait_for(
            lambda: plamenu_api.get_status(submission["id"])["downvotes_count"] == 0,
            desc="the Undo(Dislike) to clear the downvote",
        )
        wait_for(
            lambda: plamenu2_api.get_status(theirs["id"])["downvotes_count"] == 0,
            desc="the cleared score to be reflected for the voter too",
        )


@pytest.mark.federation(direction="both")
def test_group_moderation_reaches_the_members(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """A moderator acts on the host; the member's server must follow suit.

    Covers the half no Lemmy test can reach, because there Lemmy is the one
    applying the verbs: `Lock`/`Undo(Lock)` reaching a member's own copy of
    its own submission, and a mod removal retracting the group attribution on
    the member's side while leaving the author's post itself alone — a remote
    moderator governs its community, not somebody else's server."""
    name = unique("plam2mod")

    with step("a group exists locally and the peer's user joins and posts"):
        cli.group_add(name, owner=plamenu_user.username)
        group = interop.resolve_group(plamenu2_api, f"{name}@{plamenu_user.domain}")
        plamenu2_api.follow(group["id"])
        wait_for(
            lambda: plamenu2_api.relationship(group["id"])["following"],
            desc="the group's Accept(Follow) to reach the joining member",
        )
        submission = plamenu2_api.post_status(
            f"mod body {marker}", title=f"Mod thread {marker}", group_id=group["id"]
        )
        hosted = wait_for(
            lambda: group_post_by_title(
                plamenu_api, plamenu_api.lookup(name)["id"], f"Mod thread {marker}"
            ),
            desc="the submission to be announced by the group",
        )

    with step("locking the thread locks it for the member too"):
        cli.group_lock(name, hosted["id"])
        wait_for(
            lambda: plamenu2_api.get_status(submission["id"])["group_locked"] is True,
            desc="the group's Lock to reach the member's copy",
        )

    with step("unlocking reopens it on both sides"):
        cli.group_lock(name, hosted["id"], unlock=True)
        wait_for(
            lambda: plamenu2_api.get_status(submission["id"])["group_locked"] is False,
            desc="the group's Undo(Lock) to reach the member's copy",
        )

    with step("a moderator removal retracts the thread from the group"):
        cli.group_remove(name, hosted["id"])
        wait_for(
            lambda: (
                group_post_by_title(
                    plamenu_api, plamenu_api.lookup(name)["id"], f"Mod thread {marker}"
                )
                is None
            ),
            desc="the removal to drop the thread from the group's own timeline",
        )
        wait_for(
            lambda: plamenu2_api.get_status(submission["id"])["group_post"] is False,
            desc="the Undo(Announce) to retract the attribution on the member's side",
        )

    with step("…but the author's own post survives it"):
        # A community moderator governs the community. The author's server is
        # not theirs to delete from.
        still_there = plamenu2_api.get_status(submission["id"])
        assert marker in still_there["content"], still_there


@pytest.mark.federation(direction="both")
def test_group_rename_and_deletion_reach_the_members(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """The group actor's own lifecycle, as a remote member sees it.

    Covers: `Update(Group)` refreshing the member's copy of the group profile,
    and `Delete(Group)` retiring it — after which the group is gone from the
    member's instance rather than lingering as a joinable actor."""
    name = unique("plam2life")

    with step("a group exists locally and the peer's user joins it"):
        cli.group_add(name, owner=plamenu_user.username, display_name="Before")
        group = interop.resolve_group(plamenu2_api, f"{name}@{plamenu_user.domain}")
        assert group["display_name"] == "Before", group
        plamenu2_api.follow(group["id"])
        wait_for(
            lambda: plamenu2_api.relationship(group["id"])["following"],
            desc="the group's Accept(Follow) to reach the joining member",
        )

    with step("renaming it refreshes the member's copy"):
        new_title = f"Renamed {marker}"
        cli.group_rename(name, display_name=new_title)
        wait_for(
            lambda: (
                (plamenu2_api.account(group["id"]) or {})["display_name"] == new_title
            ),
            desc="the Update(Group) to refresh the member's copy",
        )

    with step("deleting it retires the group on the member's instance"):
        cli.group_delete(name)
        wait_for(
            lambda: (
                (account := plamenu2_api.account(group["id"])) is None
                or account.get("suspended") is True
            ),
            desc="the Delete(Group) to retire the group for its members",
        )
