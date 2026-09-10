"""Federation between Plamenu and GoToSocial, both directions.

GoToSocial is the strictest mainstream peer: it enforces authorized fetch on
every AP endpoint (so each resolve here also exercises Plamenu's signed
GETs), new accounts default to manual follow approval, and its feature set
diverges from Mastodon's — no emoji reactions and no quote posts at all in
0.22. The tests cover the shared surface (follow, post, edit, delete, poll,
favourite, boost, block, report, move) plus graceful degradation where GtS
lacks the feature (reactions, quotes).

`dave` is the standing GtS admin (created by `./dev up gotosocial`); every
Plamenu-side account is a fresh throwaway, so tests stay independent.
"""

import json

import pytest
from plamenu_e2e import config, gotosocial, mastodon, unique
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.media import cached_attachment, make_png, tiny_png
from plamenu_e2e.steps import log, step, wait_for

DAVE = f"dave@{config.GOTOSOCIAL_DOMAIN}"


def _dave_follows(gts_dave: Api, acct: str) -> dict:
    """Have dave follow `acct` (auto-accepted: Plamenu accounts are unlocked);
    the resolved GtS account entity."""
    account = gts_dave.resolve_account(acct)
    assert account, f"dave cannot resolve {acct}"
    gts_dave.follow(account["id"])
    wait_for(
        lambda: gts_dave.relationship(account["id"])["following"],
        desc=f"dave's follow of {acct} to be accepted",
    )
    return account


def _follow_dave(plamenu_api: Api, gts_dave: Api) -> dict:
    """Have a Plamenu user follow dave, riding through GtS's manual-approval
    default; the resolved Plamenu account entity for dave."""
    account = plamenu_api.resolve_account(DAVE)
    assert account, "plamenu cannot resolve dave"
    rel = plamenu_api.follow(account["id"])
    if not rel["following"]:
        assert rel["requested"], f"follow neither live nor pending: {rel!r}"
        wait_for(
            lambda: gotosocial.accept_follows(gts_dave) or None,
            desc="dave to see (and accept) the pending follow request",
        )
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="dave's Accept to reach plamenu (following=true)",
        )
    return account


# ── discovery ─────────────────────────────────────────────────────────


@pytest.mark.federation(direction="both")
def test_discovery_both_directions(gts_dave, plamenu_user, plamenu_api):
    """Webfinger + actor fetch in both directions. GtS enforces authorized
    fetch, so the Plamenu-side resolve only works with a valid signed GET."""
    with step(f"GtS resolves @{plamenu_user.acct}"):
        account = gts_dave.resolve_account(plamenu_user.acct)
        assert account, f"GtS could not resolve {plamenu_user.acct}"
        assert account["acct"] == plamenu_user.acct

    with step(f"Plamenu resolves @{DAVE} (signed fetch past authorized-fetch)"):
        account = plamenu_api.resolve_account(DAVE)
        assert account, "plamenu could not resolve dave"
        assert account["acct"] == DAVE
        log(f"resolved to plamenu account id {account['id']}")


# ── follow ────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_follows_gts_with_manual_approval"
)
def test_gts_follows_and_unfollows_plamenu_user(gts_dave, plamenu_user, db):
    """Inbound: signed Follow -> Plamenu auto-Accept -> follower row; then
    Undo(Follow) removes it."""
    with step(f"dave follows @{plamenu_user.acct}"):
        account = _dave_follows(gts_dave, plamenu_user.acct)
        assert db.follower_count(plamenu_user.username) == 1

    with step("dave appears in the followers listing"):
        local = Api(config.PLAMENU_URL).lookup(plamenu_user.username)
        followers = Api(config.PLAMENU_URL).followers(local["id"])
        assert [f["acct"] for f in followers] == [DAVE], followers

    with step("unfollow: Undo(Follow) -> plamenu inbox"):
        gts_dave.unfollow(account["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the follow row to disappear after Undo",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_follows_and_unfollows_plamenu_user"
)
def test_plamenu_follows_gts_with_manual_approval(gts_dave, plamenu_api):
    """Outbound: dave is locked (GtS default), so the Follow parks as a
    follow request; dave authorizes it and the Accept flips the relationship."""
    with step(f"follow @{DAVE}: lands as a pending follow request"):
        account = plamenu_api.resolve_account(DAVE)
        assert account, "plamenu cannot resolve dave"
        rel = plamenu_api.follow(account["id"])
        assert rel["requested"] and not rel["following"], rel

    with step("dave authorizes the request; Accept reaches plamenu"):
        accepted = wait_for(
            lambda: gotosocial.accept_follows(gts_dave) or None,
            desc="the pending follow request to appear on GtS",
        )
        log(f"accepted: {[a['acct'] for a in accepted]}")
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="plamenu relationship to become following=true",
        )

    with step("unfollow again"):
        plamenu_api.unfollow(account["id"])
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        wait_for(
            lambda: (
                not any(
                    f["acct"] == me["acct"]
                    for f in gts_dave.followers(
                        gts_dave.get("/api/v1/accounts/verify_credentials")["id"]
                    )
                )
            ),
            desc="the follower row to disappear on GtS after Undo",
        )


# ── posts: create / edit / delete ─────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_post_edit_delete_reach_gts"
)
def test_gts_post_edit_delete_reach_plamenu(gts_dave, plamenu_api, marker):
    """GtS -> Plamenu: Create lands in the follower's home timeline, Update
    rewrites it, Delete tombstones it."""
    with step("a plamenu user follows dave"):
        _follow_dave(plamenu_api, gts_dave)

    with step("dave posts; the status reaches the follower's home timeline"):
        posted = gts_dave.post_status(f"hello from gts {marker}")
        got = wait_for(
            lambda: plamenu_api.home_status_containing(marker),
            desc="dave's post to reach the plamenu home timeline",
        )

    with step("dave edits; the Update rewrites the ingested copy"):
        edited_marker = unique("gtsedit")
        gts_dave.edit_status(posted["id"], f"edited on gts {edited_marker}")
        wait_for(
            lambda: (
                edited_marker
                in (plamenu_api.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="the edit to reach plamenu",
        )

    with step("dave deletes; the copy is tombstoned"):
        gts_dave.delete_status(posted["id"])
        wait_for(
            lambda: plamenu_api.get_status_or_none(got["id"]) is None,
            desc="the deleted status to disappear from plamenu",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_post_edit_delete_reach_plamenu"
)
def test_plamenu_post_edit_delete_reach_gts(gts_dave, plamenu_api, db, marker):
    """Plamenu -> GtS: same lifecycle, opposite direction.

    Arrival is asserted via GtS's local knowledge (resolve=false search),
    not dave's home timeline: when the delete at the end hits GtS's
    edit-then-delete bug (below), the failed delete also wedges GtS's
    home-timeline fan-in until restart, and this test must stay re-runnable
    against the wedged instance.
    """
    with step("dave follows the plamenu user"):
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        _dave_follows(gts_dave, f"{me['username']}@{config.PLAMENU_DOMAIN}")

    with step("plamenu posts; the Create is ingested by GtS"):
        posted = plamenu_api.post_status(f"hello from plamenu {marker}")
        got = wait_for(
            lambda: gotosocial.known_status(gts_dave, posted["uri"]),
            desc="the plamenu post to be ingested by GtS (inbox delivery)",
        )

    with step("plamenu edits; GtS applies the Update"):
        edited_marker = unique("plamedit")
        plamenu_api.edit_status(posted["id"], f"edited on plamenu {edited_marker}")
        wait_for(
            lambda: (
                edited_marker
                in (gts_dave.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="the edit to reach GtS",
        )

    with step("plamenu deletes; the Delete is delivered"):
        plamenu_api.delete_status(posted["id"])
        wait_for(
            lambda: db.pending_deliveries_to(config.GOTOSOCIAL_DOMAIN) == 0,
            desc="the Delete delivery to GtS to drain",
        )

    with step("GtS drops its copy (known upstream bug: expected failure)"):
        # GtS 0.22 (stable and snapshot, verified 2026-07-10) fails to
        # process a remote Delete of a previously-edited status:
        #   deleteStatus: db error stubbing status …: sqlite3: constraint
        #   failed: NOT NULL constraint failed: statuses.thread_id
        # Reproducible with a Mastodon 4.6.2 origin as well, so it is not a
        # Plamenu shape problem. Plamenu's side (delivery, 202) is asserted
        # above; the GtS-side disappearance xfails until upstream fixes it.
        try:
            wait_for(
                lambda: gts_dave.get_status_or_none(got["id"]) is None,
                desc="the deleted status to disappear from GtS",
                timeout=30,
            )
        except TimeoutError:
            pytest.xfail(
                "GtS 0.22 cannot delete a remote status it has an edit for "
                "(statuses.thread_id stub constraint; Mastodon-reproducible)"
            )


# ── polls ─────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_votes_on_gts_poll"
)
def test_gts_votes_on_plamenu_poll(gts_dave, plamenu_api, marker):
    """A GtS vote (Create{Note name=option}) lands on the Plamenu author's
    poll tallies."""
    with step("plamenu posts a poll; dave resolves it"):
        posted = plamenu_api.post_poll(f"poll {marker}", ["rust", "go"])
        got = wait_for(
            lambda: gts_dave.resolve_status(posted["uri"]),
            desc="dave to resolve the plamenu poll by URI",
        )
        assert got["poll"], f"GtS ingested the poll-less status: {got!r}"

    with step("dave votes; the tally updates on plamenu"):
        gts_dave.vote(got["poll"]["id"], [1])
        wait_for(
            lambda: plamenu_api.get_poll(posted["poll"]["id"])["votes_count"] == 1,
            desc="dave's vote to land on the plamenu tally",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_votes_on_plamenu_poll"
)
def test_plamenu_votes_on_gts_poll(gts_dave, plamenu_api, marker):
    """The opposite direction: a Plamenu vote lands on GtS tallies."""
    with step("dave posts a poll; the plamenu user resolves it"):
        posted = gts_dave.post_poll(f"poll {marker}", ["cats", "dogs"])
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve the GtS poll by URL",
        )
        assert got["poll"], f"plamenu ingested the poll-less status: {got!r}"

    with step("the plamenu user votes; the tally updates on GtS"):
        plamenu_api.vote(got["poll"]["id"], [0])
        wait_for(
            lambda: gts_dave.get_poll(posted["poll"]["id"])["votes_count"] == 1,
            desc="the plamenu vote to land on the GtS tally",
        )


# ── favourites & boosts ───────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_favourite_and_boost_plamenu_to_gts"
)
def test_favourite_and_boost_gts_to_plamenu(gts_dave, plamenu_api, marker):
    """Dave favourites and boosts a Plamenu post; Like/Announce arrive as
    notifications and counters."""
    with step("plamenu posts; dave resolves the status"):
        posted = plamenu_api.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: gts_dave.resolve_status(posted["uri"]),
            desc="dave to resolve the plamenu post",
        )

    with step("dave favourites and boosts"):
        gts_dave.favourite(got["id"])
        gts_dave.reblog(got["id"])

    with step("both arrive as notifications on plamenu"):
        wait_for(
            lambda: plamenu_api.notifications_from(DAVE, "favourite"),
            desc="a favourite notification from dave",
        )
        wait_for(
            lambda: plamenu_api.notifications_from(DAVE, "reblog"),
            desc="a reblog notification from dave",
        )
        status = plamenu_api.get_status(posted["id"])
        assert status["favourites_count"] == 1, status["favourites_count"]
        assert status["reblogs_count"] == 1, status["reblogs_count"]


@pytest.mark.federation(
    direction="outbound", reverse_of="test_favourite_and_boost_gts_to_plamenu"
)
def test_favourite_and_boost_plamenu_to_gts(gts_dave, plamenu_api, marker):
    """The opposite direction: Plamenu favourites and boosts a GtS post."""
    with step("dave posts; the plamenu user resolves the status"):
        posted = gts_dave.post_status(f"fav+boost me {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve dave's post",
        )

    with step("the plamenu user favourites and boosts"):
        plamenu_api.favourite(got["id"])
        plamenu_api.reblog(got["id"])

    with step("both arrive as notifications on GtS"):
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        acct = f"{me['username']}@{config.PLAMENU_DOMAIN}"
        wait_for(
            lambda: gts_dave.notifications_from(acct, "favourite"),
            desc="a favourite notification on GtS",
        )
        wait_for(
            lambda: gts_dave.notifications_from(acct, "reblog"),
            desc="a reblog notification on GtS",
        )


# ── graceful degradation: reactions & quotes ──────────────────────────


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="graceful degradation toward a peer lacking the feature: GtS has no emoji reactions, so Plamenu's EmojiReact downgrades to a Like; no inbound counterpart.",
)
def test_reaction_toward_gts_degrades_gracefully(gts_dave, plamenu_api, db, marker):
    """GtS has no emoji reactions: the outbound EmojiReact must not poison
    the delivery queue, and later activities still go through."""
    with step("dave posts; the plamenu user reacts with 🔥"):
        posted = gts_dave.post_status(f"react to me {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve dave's post",
        )
        plamenu_api.react(got["id"], "🔥")
        assert [r["name"] for r in plamenu_api.emoji_reactions(got["id"])] == ["🔥"]

    with step("the delivery queue to GtS drains (nothing stuck retrying)"):
        wait_for(
            lambda: db.pending_deliveries_to(config.GOTOSOCIAL_DOMAIN) == 0,
            desc="pending deliveries to gotosocial.local to drain",
        )

    with step("a favourite after the reaction still federates"):
        plamenu_api.favourite(got["id"])
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        acct = f"{me['username']}@{config.PLAMENU_DOMAIN}"
        wait_for(
            lambda: gts_dave.notifications_from(acct, "favourite"),
            desc="the favourite notification on GtS (queue still healthy)",
        )


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="graceful degradation: GtS does not advertise a quote interaction policy, so Plamenu refuses the unsupported action before federation; no inbound counterpart.",
)
def test_quote_of_gts_post_is_refused(gts_dave, plamenu_api, marker):
    """GtS (0.22) has no quote posts and advertises no FEP-044f policy.
    Plamenu must refuse the quote instead of creating a post that can only stay
    pending forever."""
    with step("dave posts; Plamenu sees that the post is not quotable"):
        posted = gts_dave.post_status(f"quote me {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve dave's post",
        )
        assert got["quote_approval"]["current_user"] == "denied", got

    with (
        step("a crafted client request is refused before creating a quote"),
        pytest.raises(ApiError, match=r"422.*[Qq]uot"),
    ):
        plamenu_api.post_status(f"quoting gts {marker}", quoted_status_id=got["id"])


# ── blocks ────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_block_reaches_plamenu"
)
def test_plamenu_block_severs_gts_follow(gts_dave, plamenu_api, plamenu_user):
    """Outbound Block: dave follows the user; the user's block severs the
    follow on the GtS side."""
    with step("dave follows the plamenu user"):
        account = _dave_follows(gts_dave, plamenu_user.acct)

    with step("the plamenu user blocks dave; GtS severs the follow"):
        dave_on_plamenu = plamenu_api.resolve_account(DAVE)
        plamenu_api.block(dave_on_plamenu["id"])
        wait_for(
            lambda: not gts_dave.relationship(account["id"])["following"],
            desc="dave's follow to be severed by the Block",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_block_severs_gts_follow"
)
def test_gts_block_reaches_plamenu(gts_dave, plamenu_api, plamenu_user, db):
    """Inbound Block: the user follows dave; dave's block severs it and
    records the inbound block row."""
    with step("the plamenu user follows dave"):
        account = _follow_dave(plamenu_api, gts_dave)

    with step("dave blocks the user; plamenu severs the follow"):
        target = gts_dave.resolve_account(plamenu_user.acct)
        gts_dave.block(target["id"])
        wait_for(
            lambda: db.inbound_block_count(plamenu_user.username) == 1,
            desc="the inbound block row to appear",
        )
        wait_for(
            lambda: not plamenu_api.relationship(account["id"])["following"],
            desc="the follow toward dave to be severed",
        )
        gts_dave.unblock(target["id"])


# ── reports ───────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_report_forwards_to_plamenu"
)
def test_plamenu_report_forwards_to_gts(gts_dave, plamenu_api, marker):
    """A forwarded Plamenu report reaches GtS as a Flag; dave (admin) sees
    it in the moderation queue."""
    with step("dave posts; the plamenu user reports it with forward=true"):
        posted = gts_dave.post_status(f"reportable {marker}")
        got = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve dave's post",
        )
        plamenu_api.report(
            got["account"]["id"],
            comment=f"e2e forwarded report {marker}",
            status_ids=[got["id"]],
            forward=True,
        )

    with step("the Flag shows up in GtS's admin report queue"):
        # No `resolved` filter: GtS answers that with an empty list even for
        # unresolved reports; the unfiltered listing shows them.
        wait_for(
            lambda: any(
                marker in json.dumps(r) for r in gts_dave.get("/api/v1/admin/reports")
            ),
            desc="the forwarded report to appear on GtS",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_report_forwards_to_gts"
)
def test_gts_report_forwards_to_plamenu(
    gts_dave, plamenu_user, plamenu_api, db, marker
):
    """The opposite direction: dave reports a Plamenu user with forwarding
    and the Flag lands in Plamenu's moderation queue."""
    with step("the plamenu user posts; dave reports it with forward=true"):
        posted = plamenu_api.post_status(f"reportable {marker}")
        got = wait_for(
            lambda: gts_dave.resolve_status(posted["uri"]),
            desc="dave to resolve the plamenu post",
        )
        gts_dave.report(
            got["account"]["id"],
            comment=f"e2e forwarded report {marker}",
            status_ids=[got["id"]],
            forward=True,
        )

    with step("the Flag lands in plamenu's moderation queue"):
        wait_for(
            lambda: db.inbound_report_count(plamenu_user.username, marker) == 1,
            desc="the forwarded report row to appear on plamenu",
        )


# ── move ──────────────────────────────────────────────────────────────


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="outbound server-lifecycle Move: a Plamenu user Moves and its GtS follower re-follows the target; the inbound GtS-originated Move companion is unrunnable (see test_gts_move_reaches_plamenu).",
)
def test_move_makes_gts_follower_refollow(gts_dave, plamenu_user, cli):
    """Account migration: dave follows a Plamenu account that then moves to a
    fresh Mastodon account; GtS processes the Move by refollowing the new
    home (its old follow is retired)."""
    target_username = unique("moved")

    with step("create the Mastodon target and let it claim this account"):
        mastodon.create_account(target_username)
        alias_uri = mastodon.add_alias(target_username, plamenu_user.acct)
        assert alias_uri.startswith(config.PLAMENU_URL), alias_uri

    with step("dave follows the plamenu account"):
        old = _dave_follows(gts_dave, plamenu_user.acct)

    with step("the plamenu account migrates (CLI `account migrate`)"):
        cli.alias_add(
            plamenu_user.username, f"{target_username}@{config.MASTODON_DOMAIN}"
        )
        out = cli.migrate(
            plamenu_user.username, f"{target_username}@{config.MASTODON_DOMAIN}"
        )
        log(out)

    with step("GtS refollows the new home"):
        target = wait_for(
            lambda: gts_dave.resolve_account(
                f"{target_username}@{config.MASTODON_DOMAIN}"
            ),
            desc="dave to resolve the move target",
        )
        wait_for(
            lambda: (
                (rel := gts_dave.relationship(target["id"]))["following"]
                or rel["requested"]
            ),
            desc="dave to (re)follow the move target",
        )
        wait_for(
            lambda: not gts_dave.relationship(old["id"])["following"],
            desc="dave's follow of the old account to be retired",
        )


# ── replies & mentions ────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_reply_and_mention_reach_gts"
)
def test_gts_reply_and_mention_reach_plamenu(
    gts_dave, plamenu_user, plamenu_api, db, marker
):
    """GtS -> Plamenu: a reply mentioning the local author threads under the
    parent, notifies the author, and shows in replies_count / context."""
    with step("plamenu posts; dave resolves the status by URL"):
        posted = plamenu_api.post_status(f"reply to me from plamenu {marker}")
        got = wait_for(
            lambda: gts_dave.resolve_status(posted["uri"]),
            desc="dave to resolve the plamenu status",
        )

    with step("dave replies, mentioning the plamenu user"):
        gts_dave.post_status(
            f"@{plamenu_user.acct} a reply from gts {marker}r", in_reply_to_id=got["id"]
        )

    with step("the reply arrives threaded under the plamenu post"):
        reply_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the GtS reply to arrive",
        )
        assert db.status_parent_id(reply_id) == int(posted["id"])

    with step("the author gets a mention notification carrying the reply"):
        notifs = wait_for(
            lambda: plamenu_api.notifications_from(DAVE, "mention"),
            desc="a mention notification from dave",
        )
        assert notifs[0]["status"]["id"] == str(reply_id)
        assert plamenu_user.username in [
            m["acct"] for m in notifs[0]["status"]["mentions"]
        ]
        assert plamenu_api.get_status(posted["id"])["replies_count"] >= 1
        assert any(
            f"{marker}r" in s["content"]
            for s in plamenu_api.context(posted["id"])["descendants"]
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_reply_and_mention_reach_plamenu"
)
def test_plamenu_reply_and_mention_reach_gts(
    gts_dave, plamenu_user, plamenu_api, marker
):
    """Plamenu -> GtS: a reply mentioning dave threads under his post and
    notifies him (resolving dave's post exercises Plamenu's signed fetch past
    GtS authorized-fetch)."""
    with step("dave posts; plamenu resolves the status by URL"):
        posted = gts_dave.post_status(f"reply to me from gts {marker}")
        local = wait_for(
            lambda: plamenu_api.resolve_status(posted["uri"]),
            desc="plamenu to resolve dave's status",
        )

    with step("plamenu replies, mentioning dave"):
        reply = plamenu_api.post_status(
            f"@{DAVE} a reply from plamenu {marker}r", in_reply_to_id=local["id"]
        )
        assert reply["in_reply_to_id"] == local["id"]
        assert DAVE in [m["acct"] for m in reply["mentions"]]

    with step("dave gets a mention notification threaded onto his post"):
        notifs = wait_for(
            lambda: gts_dave.notifications_from(plamenu_user.acct, "mention"),
            desc="a mention notification on GtS",
        )
        assert notifs[0]["status"]["in_reply_to_id"] == posted["id"]

    with step("his replies_count and context reflect the reply"):
        wait_for(
            lambda: gts_dave.get_status(posted["id"])["replies_count"] >= 1,
            desc="dave's replies_count to reach 1",
        )
        assert any(
            f"{marker}r" in s["content"]
            for s in gts_dave.context(posted["id"])["descendants"]
        )


# ── profile updates ───────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_profile_edit_reaches_gts"
)
def test_gts_profile_edit_reaches_plamenu(
    gts_dave, plamenu_user, plamenu_api, db, marker
):
    """GtS -> Plamenu: dave's profile Update refreshes the stored remote
    account (display name + avatar). GtS fans Update(Actor) to followers, so a
    plamenu follower of dave is required."""
    with step("a plamenu user follows dave (rides GtS manual approval)"):
        _follow_dave(plamenu_api, gts_dave)

    with step("dave edits his profile on GtS"):
        new_name = f"Dave {marker}"
        gts_dave.update_profile(
            files={"avatar": ("a.png", make_png(64, 64, (30, 30, 200)), "image/png")},
            display_name=new_name,
        )

    with step("Plamenu applies the Update(Actor)"):
        wait_for(
            lambda: (
                db.remote_display_name("dave", config.GOTOSOCIAL_DOMAIN) == new_name
            ),
            desc="dave's new display name to reach plamenu",
        )
        avatar = db.remote_avatar_url("dave", config.GOTOSOCIAL_DOMAIN)
        assert avatar and avatar.startswith("https://"), avatar


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_profile_edit_reaches_plamenu"
)
def test_plamenu_profile_edit_reaches_gts(gts_dave, plamenu_user, plamenu_api, marker):
    """Plamenu -> GtS: a profile edit refreshes dave's cached copy (display
    name + note + bot flag + re-fetched avatar). dave must follow first."""
    with step("dave follows the plamenu user (so Updates reach GtS)"):
        _dave_follows(gts_dave, plamenu_user.acct)

    with step("edit the profile through Plamenu's client API"):
        new_name = f"Renamed {marker}"
        entity = plamenu_api.update_profile(
            files={"avatar": ("avatar.png", tiny_png(), "image/png")},
            display_name=new_name,
            note=f"bio {marker}",
            bot="true",
        )
        assert entity["display_name"] == new_name and entity["bot"] is True

    with step("GtS applies the Update(Actor)"):
        wait_for(
            lambda: gts_dave.lookup(plamenu_user.acct)["display_name"] == new_name,
            desc="the new display name to reach GtS",
        )
        refreshed = gts_dave.lookup(plamenu_user.acct)
        assert marker in refreshed["note"]
        assert refreshed["bot"] is True
        assert (
            "missing" not in refreshed["avatar"]
            and "default" not in refreshed["avatar"]
        )


# ── media attachments ─────────────────────────────────────────────────


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_media_reaches_gts"
)
def test_gts_media_reaches_plamenu(gts_dave, plamenu_user, plamenu_api, db, marker):
    """GtS -> Plamenu: blurhash, focal point, description and dimensions land on
    the stored row, and Plamenu re-hosts the file on its own /media/ route."""
    with step("a plamenu user follows dave"):
        _follow_dave(plamenu_api, gts_dave)

    with step("dave posts an image with description + focus"):
        up = gts_dave.upload_media(
            make_png(),
            filename="gts.png",
            mime="image/png",
            description="a gts picture",
            focus="0.4,-0.2",
        )
        gts_dave.post_with_media(f"gts media {marker}", [up["id"]])

    with step("the stored Plamenu row carries the federated metadata"):
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the GtS media post to arrive",
        )
        # The row is inserted at ingest with the federated metadata; Plamenu's
        # remote-media job then downloads the file and backfills the recomputed
        # blurhash + dimensions asynchronously — so wait for those to be present
        # (r[2]=blurhash, r[5]=width, r[6]=height), not merely for the row to
        # exist (it races the backfill under load, though it wins in isolation).
        rows = wait_for(
            lambda: (
                [r for r in db.media_for_status(status_id) if r[2] and r[5] and r[6]]
                or None
            ),
            desc="the attachment row with recomputed blurhash + dimensions",
        )
        content_type, description, blurhash, fx, fy, w, h = rows[0]
        assert content_type.startswith("image/"), content_type
        assert description == "a gts picture", description
        assert blurhash
        assert abs(fx - 0.4) < 1e-6 and abs(fy - (-0.2)) < 1e-6, (fx, fy)
        assert w and h, (w, h)

    with step("Plamenu caches it and serves it from its own /media/ route"):
        att = wait_for(
            lambda: cached_attachment(plamenu_api, status_id, require_blurhash=True),
            desc="the attachment to be cached and served locally",
        )
        assert att["url"].startswith(f"{config.PLAMENU_URL}/media/"), att["url"]
        assert att["blurhash"]


@pytest.mark.federation(
    direction="outbound", reverse_of="test_gts_media_reaches_plamenu"
)
def test_plamenu_media_reaches_gts(gts_dave, plamenu_user, plamenu_api, marker):
    """Plamenu -> GtS: the outgoing Document's blurhash + focal point + alt text
    survive; GtS re-computes the blurhash and preserves the focal point."""
    with step("dave follows the plamenu user"):
        _dave_follows(gts_dave, plamenu_user.acct)

    with step("upload an image with description + focus on Plamenu"):
        up = plamenu_api.upload_media(
            make_png(),
            filename="pic.png",
            mime="image/png",
            description="an e2e picture",
            focus="-0.5,0.3",
        )
        assert up["blurhash"] and up["meta"]["focus"]["x"] == -0.5

    with step("post it; dave's copy carries our blurhash and focus"):
        plamenu_api.post_with_media(f"a picture {marker}", [up["id"]])
        status = wait_for(
            lambda: gts_dave.home_status_containing(marker),
            desc="the media post to appear on dave's home timeline",
        )
        att = status["media_attachments"][0]
        assert att["blurhash"]
        assert att["description"] == "an e2e picture"
        assert abs(att["meta"]["focus"]["x"] - (-0.5)) < 1e-6
        assert abs(att["meta"]["focus"]["y"] - 0.3) < 1e-6


# ── direct messages ───────────────────────────────────────────────────


@pytest.mark.federation(direction="both")
def test_direct_messages_between_gts_and_plamenu(
    gts_dave, plamenu_user, plamenu_api, db, marker
):
    """DMs both ways (GtS 0.22 supports them). The recipient follows the sender
    first (both peers filter stranger DMs out of conversations); the reply
    threads into the same conversation and reads away."""
    with step("dave follows the plamenu user (strangers' DMs are filtered)"):
        _dave_follows(gts_dave, plamenu_user.acct)

    with step("the plamenu user sends dave a private mention"):
        posted = plamenu_api.post_status(
            f"@{DAVE} psst, just for you {marker}", visibility="direct"
        )
        assert posted["visibility"] == "direct"
        mine = plamenu_api.conversation_containing(marker)
        assert mine and mine["unread"] is False

    with step("dave receives it as a direct conversation on GtS"):
        conv = wait_for(
            lambda: gts_dave.conversation_containing(marker),
            desc="the DM to appear in dave's GtS conversations",
        )
        last = conv["last_status"]
        assert last["visibility"] == "direct"

    with step("dave replies privately; Plamenu stores it as a direct status"):
        gts_dave.post_status(
            f"@{plamenu_user.acct} got it {marker}r",
            visibility="direct",
            in_reply_to_id=last["id"],
        )
        status_id = wait_for(
            lambda: db.status_id_containing(f"{marker}r"),
            desc="the GtS DM to arrive in plamenu",
        )
        assert db.status_visibility(status_id) == "direct"

    with step("the reply joins the same conversation, unread, and reads away"):
        threaded = wait_for(
            lambda: plamenu_api.conversation_containing(f"{marker}r"),
            desc="the reply to show up in the plamenu user's conversations",
        )
        assert threaded["id"] == mine["id"]
        assert threaded["unread"] is True
        assert any(a["acct"] == DAVE for a in threaded["accounts"])
        assert plamenu_api.read_conversation(threaded["id"])["unread"] is False


# ── move (inbound) ────────────────────────────────────────────────────


@pytest.mark.skip(
    reason="GtS irreversibly locks a moved account (Account.IsMoving() gates "
    "post/follow/search/vote/upload with no un-move API), so originating a Move "
    "from the standing `dave` would brick every other GtS test and cannot be "
    "reset between runs. Plamenu's inbound Move handling is proven by "
    "test_migration.py::test_mastodon_move_reaches_plamenu."
)
@pytest.mark.federation(
    direction="inbound",
    one_way_reason="GtS irreversibly locks a moved account (Account.IsMoving gates all client actions with no un-move API), so an inbound GtS-originated Move cannot be run against the standing fixture; skipped. Plamenu's inbound Move handling is proven by test_mastodon_move_reaches_plamenu.",
)
def test_gts_move_reaches_plamenu(gts_dave, plamenu_user, plamenu_api, cli):
    """Inbound Move from GtS — DOCUMENTED HARNESS LIMITATION (see skip reason).
    Would require a dedicated disposable GtS mover + a DB reset of moved_to_uri
    between runs, neither of which the stack provides."""
