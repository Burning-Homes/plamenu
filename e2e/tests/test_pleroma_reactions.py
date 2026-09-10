"""Emoji reactions against Pleroma (Pleroma-extension track P1).

P1a covers inbound reactions from Pleroma to Plamenu; P1b covers Plamenu's
local reaction API and outbound `EmojiReact`/`Undo(EmojiReact)` delivery back
to Pleroma. The custom-emote tests extend both directions to reactions whose
emoji is a *custom* one (an `Emoji` tag riding the `EmojiReact`), and the
aggregation test covers reactors from two instances merging into one group.
"""

import base64
import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import config, plamenu, pleroma, unique
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for

# A 1x1 transparent PNG — small enough for every emoji size limit.
PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhf"
    "DwAChwGA60e6kgAAAABJRU5ErkJggg=="
)

# The suite-managed custom emoji on the Pleroma side: a fixed shortcode, so
# re-runs are idempotent (ensure_custom_emoji tolerates "already exists").
PLEROMA_EMOTE = "e2eblob"


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_reacts_to_pleroma_status"
)
def test_pleroma_reaction_surfaces_on_plamenu(pleroma_bob, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu user posts a public status"):
        status = plamenu_api.post_status(
            "react to me from pleroma", visibility="public"
        )
        status_uri = status["uri"]
        log(f"status {status['id']} at {status_uri}")

    with step("pleroma bob resolves the post and reacts with 🔥"):
        resolved = pleroma_bob.search(status_uri, resolve=True, type="statuses")
        found = resolved["statuses"]
        assert found, f"Pleroma could not resolve {status_uri}"
        pleroma_id = found[0]["id"]
        pleroma_bob.put(f"/api/v1/pleroma/statuses/{pleroma_id}/reactions/🔥")

    with step("the reaction federates and shows on the Plamenu status"):

        def reactions():
            return (
                plamenu_api.get_status(status["id"])
                .get("pleroma", {})
                .get("emoji_reactions", [])
            )

        wait_for(
            lambda: any(r["name"] == "🔥" and r["count"] >= 1 for r in reactions()),
            desc="🔥 reaction to appear in pleroma.emoji_reactions",
        )
        log(f"reactions now: {reactions()}")

    with step("the author got a pleroma:emoji_reaction notification"):
        notifs = plamenu_api.get("/api/v1/notifications")
        reaction_notifs = [n for n in notifs if n["type"] == "pleroma:emoji_reaction"]
        assert reaction_notifs, (
            f"no reaction notification, got {[n['type'] for n in notifs]}"
        )
        assert reaction_notifs[0].get("emoji") == "🔥", reaction_notifs[0]


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_reaction_surfaces_on_plamenu"
)
def test_plamenu_reacts_to_pleroma_status(pleroma_bob, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("pleroma bob posts a public status"):
        pleroma_status = pleroma_bob.post_status("react to me from plamenu")
        status_uri = pleroma_status["uri"]
        log(f"pleroma status {pleroma_status['id']} at {status_uri}")

    with step("plamenu resolves the Pleroma status"):
        resolved = plamenu_api.search(status_uri, resolve=True, type="statuses")
        found = resolved["statuses"]
        assert found, f"Plamenu could not resolve {status_uri}"
        plamenu_status_id = found[0]["id"]
        log(f"resolved to Plamenu status id {plamenu_status_id}")

    with step("plamenu reacts with 🔥 and federates EmojiReact to Pleroma"):
        reacted = plamenu_api.put(
            f"/api/v1/pleroma/statuses/{plamenu_status_id}/reactions/🔥"
        )
        local_reactions = reacted.get("pleroma", {}).get("emoji_reactions", [])
        assert any(
            r["name"] == "🔥" and r["count"] >= 1 and r["me"] for r in local_reactions
        ), local_reactions

        def pleroma_reactions():
            status = pleroma_bob.get_status(pleroma_status["id"])
            return status.get("pleroma", {}).get("emoji_reactions", [])

        wait_for(
            lambda: any(
                r["name"] == "🔥" and r["count"] >= 1 for r in pleroma_reactions()
            ),
            desc="Plamenu's 🔥 reaction to appear on Pleroma",
        )
        log(f"pleroma reactions now: {pleroma_reactions()}")

    with step("plamenu deletes the reaction and federates Undo(EmojiReact)"):
        plamenu_api.delete(f"/api/v1/pleroma/statuses/{plamenu_status_id}/reactions/🔥")
        wait_for(
            lambda: (
                not any(
                    r["name"] == "🔥" and r["count"] >= 1 for r in pleroma_reactions()
                )
            ),
            desc="Plamenu's 🔥 reaction to disappear from Pleroma",
        )


def _emote_group(api, status_id):
    """The reaction group of the suite's Pleroma emote, whichever way it is
    displayed: `shortcode@domain` when the emote's image survived ingestion
    (the fixed behaviour), bare `shortcode` if the Emoji tag was dropped."""
    names = (PLEROMA_EMOTE, f"{PLEROMA_EMOTE}@{config.PLEROMA_DOMAIN}")
    return next((r for r in api.emoji_reactions(status_id) if r["name"] in names), None)


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_custom_emote_reaction_on_pleroma"
)
def test_pleroma_custom_emote_reaction_on_plamenu(pleroma_bob, plamenu_user, db):
    """Covers: inbound `EmojiReact` whose emoji is a Pleroma custom emote —
    the `:shortcode:` reaction is stored and surfaced, the emote itself is
    ingested into `custom_emojis`, the author is notified, and inbound
    `Undo(EmojiReact)` retracts it. (The emote's image URL on the reaction
    group is covered by the dedicated test below.)"""
    plamenu_api = plamenu.login(plamenu_user)

    with step(f"ensure the custom emote :{PLEROMA_EMOTE}: exists on Pleroma"):
        admin = pleroma.admin_bob()
        pleroma.ensure_custom_emoji(admin, PLEROMA_EMOTE, base64.b64decode(PNG_BASE64))

    with step("plamenu user posts; pleroma bob reacts with the custom emote"):
        status = plamenu_api.post_status("custom emote me from pleroma")
        resolved = pleroma_bob.resolve_status(status["uri"])
        assert resolved, f"Pleroma could not resolve {status['uri']}"
        pleroma_bob.react(resolved["id"], PLEROMA_EMOTE)

    with step("the reaction arrives under the emote's shortcode"):
        group = wait_for(
            lambda: _emote_group(plamenu_api, status["id"]),
            desc=f"the :{PLEROMA_EMOTE}: reaction to appear on the Plamenu status",
        )
        assert group["count"] == 1, group
        rows = db.reaction_rows(int(status["id"]))
        assert rows and rows[0][0] == PLEROMA_EMOTE, rows

    with step("the tag's Emoji rode along into custom_emojis"):
        image = db.emoji_image_url(PLEROMA_EMOTE, config.PLEROMA_DOMAIN)
        assert image and image.startswith("https://"), (
            f"the EmojiReact tag must ingest the emote: {image!r}"
        )

    with step("the author got a pleroma:emoji_reaction notification for it"):
        notifs = [
            n
            for n in plamenu_api.notifications()
            if n["type"] == "pleroma:emoji_reaction"
        ]
        assert notifs, "no reaction notification arrived"
        assert PLEROMA_EMOTE in notifs[0].get("emoji", ""), notifs[0]

    # NOTE: bob's *unreact* is deliberately not asserted here — Pleroma sends
    # `Undo` with a bare object URI; that retraction is covered by the
    # dedicated test below.


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_pleroma_reaction_surfaces_on_plamenu: an inbound Undo carrying a bare object URI still retracts the reaction; no reverse.",
)
def test_pleroma_undo_with_bare_object_uri_retracts(pleroma_bob, plamenu_user):
    """Covers: an inbound `Undo` whose `object` is the original activity's
    *id* (Pleroma's wire shape) retracts the reaction, by looking the
    activity up by its stored `uri`."""
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu posts; bob reacts 🔥 then unreacts"):
        status = plamenu_api.post_status("undo me from pleroma")
        resolved = pleroma_bob.resolve_status(status["uri"])
        assert resolved, f"Pleroma could not resolve {status['uri']}"
        pleroma_bob.react(resolved["id"], "🔥")
        wait_for(
            lambda: any(
                r["name"] == "🔥" for r in plamenu_api.emoji_reactions(status["id"])
            ),
            desc="the 🔥 reaction to arrive before the undo",
        )
        pleroma_bob.unreact(resolved["id"], "🔥")

    with step("the bare-URI Undo retracts the reaction"):
        # Short poll on purpose: a fixed implementation retracts within a
        # delivery round-trip; only the expected failure waits this out.
        wait_for(
            lambda: (
                not any(
                    r["name"] == "🔥" for r in plamenu_api.emoji_reactions(status["id"])
                )
            ),
            desc="the 🔥 reaction to be retracted by the bare-URI Undo",
            timeout=30,
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_pleroma_custom_emote_reaction_on_plamenu: the ingested custom emoji keeps its image URL; no reverse.",
)
def test_pleroma_custom_emote_reaction_keeps_its_image(pleroma_bob, plamenu_user, db):
    """Covers: the reaction group of an inbound custom emote carries the
    emote's image `url` and the Pleroma-style `shortcode@domain` display
    name, and the `status_reactions` row records `custom_emoji_url`."""
    plamenu_api = plamenu.login(plamenu_user)

    with step("ensure the emote, post, react"):
        admin = pleroma.admin_bob()
        pleroma.ensure_custom_emoji(admin, PLEROMA_EMOTE, base64.b64decode(PNG_BASE64))
        status = plamenu_api.post_status("custom emote image check")
        resolved = pleroma_bob.resolve_status(status["uri"])
        assert resolved, f"Pleroma could not resolve {status['uri']}"
        pleroma_bob.react(resolved["id"], PLEROMA_EMOTE)
        group = wait_for(
            lambda: _emote_group(plamenu_api, status["id"]),
            desc="the custom emote reaction to arrive",
        )

    with step("the group carries the image URL and the @domain display name"):
        assert group.get("url"), f"reaction group lost the emote image: {group}"
        assert group["name"] == f"{PLEROMA_EMOTE}@{config.PLEROMA_DOMAIN}", group
        rows = db.reaction_rows(int(status["id"]))
        assert rows and rows[0][1], (
            f"status_reactions.custom_emoji_url must be recorded: {rows}"
        )


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="same-direction refinement of test_plamenu_reacts_to_pleroma_status: Plamenu joins an existing Pleroma custom-emoji reaction (aggregation), not a distinct direction.",
)
def test_plamenu_joins_pleroma_custom_reaction(pleroma_bob, plamenu_user, db):
    """Covers: joining (+1) an existing remote custom-emoji reaction — bob
    reacts with his emote, the plamenu author joins it by its qualified
    `shortcode@domain` name, the join's `EmojiReact` (the remote image riding
    its `Emoji` tag) federates back to Pleroma, and the qualified unreact
    retracts just the join. Mirrors Pleroma's own join-only remote-emoji
    rule."""
    plamenu_api = plamenu.login(plamenu_user)
    display_name = f"{PLEROMA_EMOTE}@{config.PLEROMA_DOMAIN}"

    with step("ensure the emote; bob follows the author (to receive the join)"):
        admin = pleroma.admin_bob()
        pleroma.ensure_custom_emoji(admin, PLEROMA_EMOTE, base64.b64decode(PNG_BASE64))
        account = pleroma_bob.resolve_account(plamenu_user.acct)
        assert account, "Pleroma could not resolve the Plamenu account"
        pleroma_bob.follow(account["id"])
        wait_for(
            lambda: pleroma_bob.relationship(account["id"])["following"],
            desc="bob's follow of the author to be accepted",
        )

    with step("author posts; bob reacts with the custom emote"):
        status = plamenu_api.post_status("join my reaction")
        # The follow makes the post arrive by delivery too; resolving right
        # after posting races that ingestion, so poll instead of asserting.
        resolved = wait_for(
            lambda: pleroma_bob.resolve_status(status["uri"]),
            desc="Pleroma to resolve the author's post",
        )
        pleroma_bob.react(resolved["id"], PLEROMA_EMOTE)
        wait_for(
            lambda: _emote_group(plamenu_api, status["id"]),
            desc="the emote reaction to arrive on Plamenu",
        )

    with step("the author joins it by the qualified name"):
        joined = plamenu_api.react(status["id"], display_name)
        groups = (joined.get("pleroma") or {}).get("emoji_reactions", [])
        mine = next((r for r in groups if r["name"] == display_name), None)
        assert mine and mine["count"] == 2 and mine["me"], groups
        assert mine.get("url"), f"the join must keep the remote image: {mine}"

    def bob_group():
        return next(
            (
                r
                for r in pleroma_bob.emoji_reactions(resolved["id"])
                if PLEROMA_EMOTE in r["name"]
            ),
            None,
        )

    with step("the join federates back: Pleroma's group reaches count 2"):
        wait_for(
            lambda: (bob_group() or {}).get("count", 0) >= 2,
            desc="the emote group on Pleroma to reach count 2",
        )

    with step("unreact by the qualified name retracts only the join"):
        plamenu_api.unreact(status["id"], display_name)
        group = _emote_group(plamenu_api, status["id"])
        assert group and group["count"] == 1 and not group["me"], group
        wait_for(
            lambda: (bob_group() or {}).get("count", 2) == 1,
            desc="Pleroma's group to drop back to count 1",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_pleroma_reaction_surfaces_on_plamenu: Pleroma joins a Plamenu-originated custom-emoji reaction (aggregation); no reverse.",
)
def test_pleroma_joins_plamenu_custom_reaction(pleroma_bob, plamenu_user, cli):
    """Covers the reverse of test_plamenu_joins_pleroma_custom_reaction: bob
    joins (+1) an existing Plamenu-*local* custom-emoji reaction. The author
    reacts with the bare local shortcode, which seeds the emote into Akkoma via
    the `EmojiReact`'s `Emoji` tag (proven by the outbound custom-emote test);
    bob then joins by the qualified `shortcode@plamenu-domain`, and the join
    aggregates into count 2 on Plamenu.

    Akkoma's PUT /reactions/{name} is strict about qualified names — it 400s an
    unknown or not-yet-ingested emoji — so the join call is guarded with
    `pytest.xfail`, the suite's convention for an upstream quirk (cf. the GtS
    edit-then-delete xfail)."""
    plamenu_api = plamenu.login(plamenu_user)
    shortcode = unique("plamote")

    with step(f"create a local plamenu emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("bob follows the author (to receive the reaction join)"):
        account = pleroma_bob.resolve_account(plamenu_user.acct)
        assert account, "Pleroma could not resolve the Plamenu account"
        pleroma_bob.follow(account["id"])
        wait_for(
            lambda: pleroma_bob.relationship(account["id"])["following"],
            desc="bob's follow of the author to be accepted",
        )

    display_name = f"{shortcode}@{config.PLAMENU_DOMAIN}"

    with step(
        "author posts and reacts with the BARE local emoji (seeds it into Akkoma)"
    ):
        status = plamenu_api.post_status("join my local reaction")
        plamenu_api.react(status["id"], shortcode)
        resolved = wait_for(
            lambda: pleroma_bob.resolve_status(status["uri"]),
            desc="Pleroma to resolve the author's post",
        )
        wait_for(
            lambda: any(
                display_name in r["name"]
                for r in pleroma_bob.emoji_reactions(resolved["id"])
            ),
            desc=f"the {display_name} reaction (seeded from the EmojiReact) to appear on Akkoma",
        )

    with step("bob JOINS the reaction by the qualified name"):
        try:
            pleroma_bob.react(resolved["id"], display_name)
        except ApiError as exc:
            pytest.xfail(
                f"Akkoma remote-emoji join quirk (400 on qualified name): {exc}"
            )

    with step("the join aggregates into count 2 on Plamenu"):
        group = wait_for(
            lambda: next(
                (
                    r
                    for r in plamenu_api.emoji_reactions(status["id"])
                    if r["name"] == shortcode and r["count"] >= 2
                ),
                None,
            ),
            desc="the Plamenu reaction group to reach count 2",
        )
        assert group["count"] == 2, group


@pytest.mark.federation(
    direction="outbound", reverse_of="test_pleroma_custom_emote_reaction_on_plamenu"
)
def test_plamenu_custom_emote_reaction_on_pleroma(pleroma_bob, plamenu_user, cli):
    """Covers: outbound `EmojiReact` with an `Emoji` tag — a reaction using a
    *local* Plamenu custom emoji reaches Pleroma with its image, shown there
    as `shortcode@plamenu-domain`, and `Undo(EmojiReact)` retracts it."""
    plamenu_api = plamenu.login(plamenu_user)
    shortcode = unique("plamote")

    with step(f"create local emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("pleroma bob posts; plamenu resolves and reacts with the emoji"):
        pleroma_status = pleroma_bob.post_status("custom emote me from plamenu")
        resolved = plamenu_api.resolve_status(pleroma_status["uri"])
        assert resolved, f"Plamenu could not resolve {pleroma_status['uri']}"
        reacted = plamenu_api.react(resolved["id"], shortcode)
        groups = (reacted.get("pleroma") or {}).get("emoji_reactions", [])
        mine = next((r for r in groups if r["name"] == shortcode), None)
        assert mine and mine["me"] and mine["url"], (
            f"local reaction group must carry me + url: {groups}"
        )

    display_name = f"{shortcode}@{config.PLAMENU_DOMAIN}"

    with step("the emote reaction shows on Pleroma with its image"):

        def pleroma_group():
            return next(
                (
                    r
                    for r in pleroma_bob.emoji_reactions(pleroma_status["id"])
                    if r["name"] == display_name
                ),
                None,
            )

        group = wait_for(
            lambda: pleroma_group(),
            desc=f"the {display_name} reaction to appear on Pleroma",
        )
        assert group["count"] == 1, group
        assert group.get("url"), f"Pleroma lost the emote image URL: {group}"

    with step("plamenu unreacts; Undo(EmojiReact) retracts it on Pleroma"):
        plamenu_api.unreact(resolved["id"], shortcode)
        wait_for(
            lambda: pleroma_group() is None,
            desc=f"the {display_name} reaction to disappear from Pleroma",
        )


@pytest.mark.federation(direction="both")
def test_reaction_counts_aggregate_across_instances(pleroma_bob, plamenu_user, cli):
    """Covers: one reaction group merging reactors from two instances — the
    local reactor and Pleroma's both land in the same 🎉 group (count 2,
    both accounts listed via the reactions endpoint), with the `me` flag
    computed per viewer."""
    author_api = plamenu.login(plamenu_user)

    with step("plamenu author posts; a second local user reacts 🎉"):
        status = author_api.post_status("aggregate reactions on me")
        other = plamenu.User()
        cli.account_add(other)
        other_api = plamenu.login(other)
        other_api.react(status["id"], "🎉")

    with step("pleroma bob reacts 🎉 to the same status"):
        resolved = pleroma_bob.resolve_status(status["uri"])
        assert resolved, f"Pleroma could not resolve {status['uri']}"
        pleroma_bob.react(resolved["id"], "🎉")

    with step("both reactors merge into one group of two"):

        def party_group():
            groups = author_api.get(
                f"/api/v1/pleroma/statuses/{status['id']}/reactions"
            )
            return next((r for r in groups if r["name"] == "🎉"), None)

        group = wait_for(
            lambda: (party_group() or {}).get("count", 0) >= 2 and party_group(),
            desc="the 🎉 group to reach count 2",
        )
        accts = sorted(a["acct"] for a in group["accounts"])
        expected = sorted(
            [other.username, f"{pleroma.BOB_NICK}@{config.PLEROMA_DOMAIN}"]
        )
        assert accts == expected, f"reactor accounts {accts} != {expected}"
        assert group["me"] is False, "the author did not react; me must be false"
        log(f"aggregated group: count={group['count']} accounts={accts}")

    with step("the me flag is per-viewer: the local reactor sees me=true"):
        groups = other_api.emoji_reactions(status["id"])
        mine = next((r for r in groups if r["name"] == "🎉"), None)
        assert mine and mine["me"] is True, groups
