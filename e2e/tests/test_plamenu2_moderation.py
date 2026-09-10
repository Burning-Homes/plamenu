"""Plamenu against Plamenu: moderation, and moving house.

The verbs here are the ones where the two servers must agree about a person
rather than about a post: a block that severs a relationship on both sides, a
report forwarded to the server that can actually act on it, a suspension the
other side mirrors and then lifts, a whole domain cut off, and an account
migrating between the two instances with its followers in tow.

Every one of these has a Mastodon-side test already. What none of them can
check is the receiving half of *our own* emission — a `Flag` landing in a
moderation queue that renders it the way we build it, a `toot:suspended`
actor update read back by a server that publishes the same field, a `Move`
followed by a server that emits the same one.

Direction is `both` throughout: identical software on either side.
"""

import pytest
from plamenu_e2e import config, interop
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(direction="both")
def test_a_block_severs_the_relationship_on_both_servers(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A block, as the blocked server is told about it.

    Covers: the mutual follow being severed on both sides (`Undo(Follow)` out,
    `Reject(Follow)` back), the delivered `Block` making the blocked side
    refuse to re-follow, and `Undo(Block)` letting the relationship be rebuilt."""
    theirs, mine = interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user blocks the peer's user"):
        relationship = plamenu_api.block(theirs["id"])
        assert relationship["blocking"] is True, relationship
        assert relationship["following"] is False, relationship
        assert relationship["followed_by"] is False, relationship

    with step("the blocked side loses the follow it had"):
        wait_for(
            lambda: not plamenu2_api.relationship(mine["id"])["following"],
            desc="the Block to sever the follow on the blocked server",
        )

    with step("and cannot rebuild it while the block stands"):

        def refollow_bounces():
            try:
                plamenu2_api.follow(mine["id"])
            except ApiError as err:
                log(f"re-follow refused: {err}")
                return True
            return not plamenu2_api.relationship(mine["id"])["following"]

        wait_for(
            refollow_bounces,
            desc="a re-follow by the blocked account to be refused",
        )

    with step("lifting the block lets them follow again"):
        plamenu_api.unblock(theirs["id"])
        wait_for(
            lambda: plamenu_api.relationship(theirs["id"])["blocking"] is False,
            desc="the local block row to clear",
        )
        plamenu2_api.follow(mine["id"])
        wait_for(
            lambda: plamenu2_api.relationship(mine["id"])["following"],
            desc="the follow to be accepted again after the unblock",
        )


@pytest.mark.federation(direction="both")
def test_a_forwarded_report_reaches_the_other_moderation_queue(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A `Flag` this server builds, read by a server that builds the same one.

    Covers: `forward: true` on a report about a remote account, the
    instance-actor-signed `Flag` reaching the origin, and the origin turning it
    into a moderation report naming the reported account, the reported posts
    and the comment — then the same in the other direction."""
    _peer_admin_user, peer_admin = plamenu2.admin()

    with step("the peer's user posts something the local user objects to"):
        theirs = interop.follow(plamenu_api, plamenu2_user.acct)
        offending = plamenu2_api.post_status(f"something objectionable {marker}")
        ours = interop.delivered(plamenu_api, marker)

    with step("the local user reports it, asking that it be forwarded"):
        report = plamenu_api.report(
            theirs["id"],
            comment=f"e2e report {marker}",
            category="spam",
            forward=True,
            status_ids=[ours["id"]],
        )
        assert report["forwarded"] is True, report
        assert report["target_account"]["acct"] == plamenu2_user.acct, report

    with step("the origin's moderators receive it, with the post attached"):
        landed = wait_for(
            lambda: next(
                (
                    row
                    for row in peer_admin.get("/api/v1/admin/reports")
                    if marker in (row.get("comment") or "")
                ),
                None,
            ),
            desc="the forwarded Flag to become a report on the origin",
        )
        # The admin surface nests Mastodon's `Admin::Account`, whose handle is
        # a (username, domain) pair rather than an `acct` string.
        assert landed["target_account"]["username"] == plamenu2_user.username, landed
        assert landed["target_account"]["domain"] is None, landed["target_account"]
        # A forwarded report is filed by the reporting *server*, not by the
        # person who filed it — Mastodon's rule, and the reason a report can be
        # forwarded at all without handing a stranger's moderators a name.
        assert landed["account"]["username"] == plamenu_user.domain, landed["account"]
        assert landed["account"]["domain"] == plamenu_user.domain, landed["account"]
        assert [s["uri"] for s in landed["statuses"]] == [offending["uri"]], landed
        log(f"report {landed['id']} filed by {landed['account']['username']}")


@pytest.mark.federation(direction="both")
def test_a_suspension_is_mirrored_and_then_lifted(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, db, marker
):
    """A reversible suspension travels as a blanked actor, not a deletion.

    Covers: the admin action federating `Update(Actor)` with `suspended: true`,
    the other server mirroring it as a *remote-origin* suspension (so its own
    moderators are not credited with it), the account staying served with
    `suspended: true` rather than tombstoned — a suspension is reversible, and
    Mastodon's serializer says so the same way — and the unsuspend clearing the
    mirror again."""
    _peer_admin_user, peer_admin = plamenu2.admin()

    with step("the local user follows the peer's user, so the update reaches it"):
        theirs = interop.follow(plamenu_api, plamenu2_user.acct)

    with step("the peer's admin suspends the account"):
        peer_local_id = plamenu2_api.get("/api/v1/accounts/verify_credentials")["id"]
        peer_admin.post(
            f"/api/v1/admin/accounts/{peer_local_id}/action",
            type="suspend",
            text=f"e2e suspension {marker}",
        )

    with step("we mirror it, and record that it was somebody else's decision"):
        wait_for(
            lambda: (
                db.remote_suspension(plamenu2_user.username, plamenu2.domain)
                == "remote"
            ),
            desc="the suspended actor update to be mirrored locally",
        )
        mirrored = plamenu_api.account(theirs["id"])
        assert mirrored is not None, "a suspension is not a deletion"
        assert mirrored["suspended"] is True, mirrored

    with step("unsuspending lifts the mirror"):
        peer_admin.post(f"/api/v1/admin/accounts/{peer_local_id}/unsuspend")
        wait_for(
            lambda: (
                db.remote_suspension(plamenu2_user.username, plamenu2.domain) is None
            ),
            desc="the lifted suspension to reach us",
        )
        assert "suspended" not in plamenu_api.account(theirs["id"])


@pytest.mark.federation(direction="both")
def test_suspending_the_whole_domain_cuts_federation_off(
    plamenu2, plamenu_admin, plamenu2_user, plamenu2_api, cli, db, marker
):
    """A domain suspension, measured at the other end of it.

    Covers: `POST/DELETE /api/v1/admin/domain_blocks`, the cutoff applying to
    cached accounts and to fresh fetches alike, queued deliveries being
    discarded rather than retried forever, and federation resuming — proven by
    a fence post on either side of the block — once it is lifted."""
    admin_user, admin_api = plamenu_admin

    with step("the peer follows the local admin, and a baseline post arrives"):
        mine = interop.follow(plamenu2_api, admin_user.acct)
        cli.post(admin_user.username, f"before the block {marker}")
        interop.delivered(plamenu2_api, f"before the block {marker}")
        theirs = admin_api.resolve_account(plamenu2_user.acct)
        assert theirs, "the peer's user must be known before the block"

    block_id = None
    try:
        with step(f"the local admin suspends {plamenu2.domain}"):
            block = admin_api.post(
                "/api/v1/admin/domain_blocks",
                domain=plamenu2.domain,
                severity="suspend",
            )
            block_id = block["id"]
            assert block["severity"] == "suspend", block

        with step("the cached account is hidden and a fresh resolve refused"):
            assert admin_api.account(theirs["id"]) is None
            assert admin_api.resolve_account(plamenu2_user.acct) is None

        with step("a post queued under the block is discarded, not retried"):
            cli.post(admin_user.username, f"during the block {marker}")
            wait_for(
                lambda: db.pending_deliveries_to(plamenu2.domain) == 0,
                desc="the blocked delivery to drain (skipped, not retried)",
            )
    finally:
        if block_id is not None:
            with step("lift the suspension"):
                admin_api.delete(f"/api/v1/admin/domain_blocks/{block_id}")

    with step("federation resumes, and the blocked post never arrived"):
        cli.post(admin_user.username, f"after the block {marker}")
        interop.delivered(plamenu2_api, f"after the block {marker}")
        assert (
            plamenu2_api.home_status_containing(f"during the block {marker}") is None
        ), "a post published under a domain suspension must never arrive"
        assert plamenu2_api.relationship(mine["id"])["following"], (
            "the follow survives a suspension that was lifted"
        )


@pytest.mark.federation(direction="both")
def test_an_account_moves_between_the_instances_with_its_followers(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """`Move`, both as the server somebody leaves and the one they arrive at.

    Covers: `alsoKnownAs` published on the destination actor, the anti-hijack
    rule that a move to an un-aliased target is refused, the `Move` reaching a
    follower's server, and that server re-pointing the follow at the new
    account by itself — the half a Mastodon test cannot show, since there the
    re-follow is Mastodon's code, not ours."""
    destination = plamenu2.account(prefix="newhome")

    with step("a follower on the peer follows the account that will move"):
        old_home = interop.follow(plamenu2_api, plamenu_user.acct)

    with (
        step("a move to an account that has not claimed us is refused"),
        pytest.raises(RuntimeError, match="(?i)alias"),
    ):
        cli.migrate(plamenu_user.username, destination.acct)

    with step("the destination account claims the old one as an alias"):
        alias_uri = plamenu2.commands.alias_add(destination.username, plamenu_user.acct)
        assert alias_uri.startswith(config.PLAMENU_URL), alias_uri
        doc = plamenu2_api.ap_get(f"/users/{destination.username}")
        assert alias_uri in doc.get("alsoKnownAs", []), doc.get("alsoKnownAs")

    with step("the move goes through and the old account points at the new one"):
        log(cli.migrate(plamenu_user.username, destination.acct))
        moved = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        assert moved.get("movedTo") == doc["id"], moved.get("movedTo")

    with step("the follower's server follows the new home on its own"):
        new_home = wait_for(
            lambda: plamenu2_api.lookup(destination.username),
            desc="the destination account to be known to the follower's server",
        )
        wait_for(
            lambda: plamenu2_api.relationship(new_home["id"])["following"],
            desc="the Move to re-point the follow at the destination account",
        )
        wait_for(
            lambda: not plamenu2_api.relationship(old_home["id"])["following"],
            desc="the follow of the emptied account to be dropped",
        )
