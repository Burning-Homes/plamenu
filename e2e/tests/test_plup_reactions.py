"""Emoji reactions against real upstream Pleroma (plup.local).

Ports the Akkoma reaction suite (test_pleroma_reactions.py) to upstream Pleroma
2.10.2. `EmojiReact` federation rides the same signed delivery path that the RFC
9421 black-hole bug broke, so these are additional regression coverage for that
fix (plamenu/RFC9421_PLEROMA_FIX_PLAN.md), plus real interop coverage for
unicode and custom-emoji reactions and cross-instance aggregation.

Upstream Pleroma may differ from the Akkoma fork; per-case quirks are marked
with a documented `pytest.xfail`/`one_way_reason` as they are discovered.
"""

import base64
import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import config, plamenu, plup, unique
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for

# A 1x1 transparent PNG — small enough for every emoji size limit.
PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhf"
    "DwAChwGA60e6kgAAAABJRU5ErkJggg=="
)

# The suite-managed custom emoji on the upstream-Pleroma side: a fixed
# shortcode, so re-runs are idempotent (ensure_custom_emoji tolerates exists).
PLUP_EMOTE = "e2eblob"


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_reacts_to_plup_status"
)
def test_plup_reaction_surfaces_on_plamenu(plup_mari, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu user posts a public status"):
        status = plamenu_api.post_status(
            "react to me from upstream pleroma", visibility="public"
        )
        status_uri = status["uri"]
        log(f"status {status['id']} at {status_uri}")

    with step("mari resolves the post and reacts with 🔥"):
        resolved = plup_mari.search(status_uri, resolve=True, type="statuses")
        found = resolved["statuses"]
        assert found, f"upstream Pleroma could not resolve {status_uri}"
        plup_id = found[0]["id"]
        plup_mari.put(f"/api/v1/pleroma/statuses/{plup_id}/reactions/🔥")

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
    direction="outbound", reverse_of="test_plup_reaction_surfaces_on_plamenu"
)
def test_plamenu_reacts_to_plup_status(plup_mari, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("mari posts a public status"):
        plup_status = plup_mari.post_status("react to me from plamenu")
        status_uri = plup_status["uri"]
        log(f"plup status {plup_status['id']} at {status_uri}")

    with step("plamenu resolves the upstream-Pleroma status"):
        resolved = plamenu_api.search(status_uri, resolve=True, type="statuses")
        found = resolved["statuses"]
        assert found, f"Plamenu could not resolve {status_uri}"
        plamenu_status_id = found[0]["id"]
        log(f"resolved to Plamenu status id {plamenu_status_id}")

    with step("plamenu reacts with 🔥 and federates EmojiReact to upstream Pleroma"):
        reacted = plamenu_api.put(
            f"/api/v1/pleroma/statuses/{plamenu_status_id}/reactions/🔥"
        )
        local_reactions = reacted.get("pleroma", {}).get("emoji_reactions", [])
        assert any(
            r["name"] == "🔥" and r["count"] >= 1 and r["me"] for r in local_reactions
        ), local_reactions

        def plup_reactions():
            status = plup_mari.get_status(plup_status["id"])
            return status.get("pleroma", {}).get("emoji_reactions", [])

        wait_for(
            lambda: any(
                r["name"] == "🔥" and r["count"] >= 1 for r in plup_reactions()
            ),
            desc="Plamenu's 🔥 reaction to appear on upstream Pleroma",
        )
        log(f"plup reactions now: {plup_reactions()}")

    with step("plamenu deletes the reaction and federates Undo(EmojiReact)"):
        plamenu_api.delete(f"/api/v1/pleroma/statuses/{plamenu_status_id}/reactions/🔥")
        wait_for(
            lambda: (
                not any(r["name"] == "🔥" and r["count"] >= 1 for r in plup_reactions())
            ),
            desc="Plamenu's 🔥 reaction to disappear from upstream Pleroma",
        )


def _emote_group(api, status_id):
    """The reaction group of the suite's upstream-Pleroma emote, whichever way it
    is displayed: `shortcode@domain` when the emote's image survived ingestion,
    bare `shortcode` if the Emoji tag was dropped."""
    names = (PLUP_EMOTE, f"{PLUP_EMOTE}@{config.PLUP_DOMAIN}")
    return next((r for r in api.emoji_reactions(status_id) if r["name"] in names), None)


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_custom_emote_reaction_on_plup"
)
def test_plup_custom_emote_reaction_on_plamenu(plup_mari, plamenu_user, db):
    """Inbound `EmojiReact` whose emoji is an upstream-Pleroma custom emote —
    the `:shortcode:` reaction is stored and surfaced, the emote itself is
    ingested into `custom_emojis`, and the author is notified."""
    plamenu_api = plamenu.login(plamenu_user)

    with step(f"ensure the custom emote :{PLUP_EMOTE}: exists on upstream Pleroma"):
        admin = plup.admin_mari()
        plup.ensure_custom_emoji(admin, PLUP_EMOTE, base64.b64decode(PNG_BASE64))

    with step("plamenu user posts; mari reacts with the custom emote"):
        status = plamenu_api.post_status("custom emote me from upstream pleroma")
        resolved = plup_mari.resolve_status(status["uri"])
        assert resolved, f"upstream Pleroma could not resolve {status['uri']}"
        plup_mari.react(resolved["id"], PLUP_EMOTE)

    with step("the reaction arrives under the emote's shortcode"):
        group = wait_for(
            lambda: _emote_group(plamenu_api, status["id"]),
            desc=f"the :{PLUP_EMOTE}: reaction to appear on the Plamenu status",
        )
        assert group["count"] == 1, group
        rows = db.reaction_rows(int(status["id"]))
        assert rows and rows[0][0] == PLUP_EMOTE, rows

    with step("the tag's Emoji rode along into custom_emojis"):
        image = db.emoji_image_url(PLUP_EMOTE, config.PLUP_DOMAIN)
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
        assert PLUP_EMOTE in notifs[0].get("emoji", ""), notifs[0]


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_plup_reaction_surfaces_on_plamenu: an inbound Undo carrying a bare object URI still retracts the reaction; no reverse.",
)
def test_plup_undo_with_bare_object_uri_retracts(plup_mari, plamenu_user):
    """An inbound `Undo` whose `object` is the original activity's *id* (Pleroma's
    wire shape) retracts the reaction, by looking the activity up by its stored
    `uri`."""
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu posts; mari reacts 🔥 then unreacts"):
        status = plamenu_api.post_status("undo me from upstream pleroma")
        resolved = plup_mari.resolve_status(status["uri"])
        assert resolved, f"upstream Pleroma could not resolve {status['uri']}"
        plup_mari.react(resolved["id"], "🔥")
        wait_for(
            lambda: any(
                r["name"] == "🔥" for r in plamenu_api.emoji_reactions(status["id"])
            ),
            desc="the 🔥 reaction to arrive before the undo",
        )
        plup_mari.unreact(resolved["id"], "🔥")

    with step("the bare-URI Undo retracts the reaction"):
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
    one_way_reason="same-direction refinement of test_plup_custom_emote_reaction_on_plamenu: the ingested custom emoji keeps its image URL; no reverse.",
)
def test_plup_custom_emote_reaction_keeps_its_image(plup_mari, plamenu_user, db):
    """The reaction group of an inbound custom emote carries the emote's image
    `url` and the Pleroma-style `shortcode@domain` display name, and the
    `status_reactions` row records `custom_emoji_url`."""
    plamenu_api = plamenu.login(plamenu_user)

    with step("ensure the emote, post, react"):
        admin = plup.admin_mari()
        plup.ensure_custom_emoji(admin, PLUP_EMOTE, base64.b64decode(PNG_BASE64))
        status = plamenu_api.post_status("custom emote image check")
        resolved = plup_mari.resolve_status(status["uri"])
        assert resolved, f"upstream Pleroma could not resolve {status['uri']}"
        plup_mari.react(resolved["id"], PLUP_EMOTE)
        group = wait_for(
            lambda: _emote_group(plamenu_api, status["id"]),
            desc="the custom emote reaction to arrive",
        )

    with step("the group carries the image URL and the @domain display name"):
        assert group.get("url"), f"reaction group lost the emote image: {group}"
        assert group["name"] == f"{PLUP_EMOTE}@{config.PLUP_DOMAIN}", group
        rows = db.reaction_rows(int(status["id"]))
        assert rows and rows[0][1], (
            f"status_reactions.custom_emoji_url must be recorded: {rows}"
        )


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="same-direction refinement of test_plamenu_reacts_to_plup_status: Plamenu joins an existing upstream-Pleroma custom-emoji reaction (aggregation), not a distinct direction.",
)
def test_plamenu_joins_plup_custom_reaction(plup_mari, plamenu_user, db):
    """Joining (+1) an existing remote custom-emoji reaction — mari reacts with
    her emote, the plamenu author joins it by its qualified `shortcode@domain`
    name, the join federates back, and the qualified unreact retracts just the
    join."""
    plamenu_api = plamenu.login(plamenu_user)
    display_name = f"{PLUP_EMOTE}@{config.PLUP_DOMAIN}"

    with step("ensure the emote; mari follows the author (to receive the join)"):
        admin = plup.admin_mari()
        plup.ensure_custom_emoji(admin, PLUP_EMOTE, base64.b64decode(PNG_BASE64))
        account = plup_mari.resolve_account(plamenu_user.acct)
        assert account, "upstream Pleroma could not resolve the Plamenu account"
        plup_mari.follow(account["id"])
        wait_for(
            lambda: plup_mari.relationship(account["id"])["following"],
            desc="mari's follow of the author to be accepted",
        )

    with step("author posts; mari reacts with the custom emote"):
        status = plamenu_api.post_status("join my reaction")
        resolved = wait_for(
            lambda: plup_mari.resolve_status(status["uri"]),
            desc="upstream Pleroma to resolve the author's post",
        )
        plup_mari.react(resolved["id"], PLUP_EMOTE)
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

    def mari_group():
        return next(
            (
                r
                for r in plup_mari.emoji_reactions(resolved["id"])
                if PLUP_EMOTE in r["name"]
            ),
            None,
        )

    with step("the join federates back: upstream Pleroma's group reaches count 2"):
        wait_for(
            lambda: (mari_group() or {}).get("count", 0) >= 2,
            desc="the emote group on upstream Pleroma to reach count 2",
        )

    with step("unreact by the qualified name retracts only the join"):
        plamenu_api.unreact(status["id"], display_name)
        group = _emote_group(plamenu_api, status["id"])
        assert group and group["count"] == 1 and not group["me"], group
        wait_for(
            lambda: (mari_group() or {}).get("count", 2) == 1,
            desc="upstream Pleroma's group to drop back to count 1",
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="same-direction refinement of test_plup_reaction_surfaces_on_plamenu: upstream Pleroma joins a Plamenu-originated custom-emoji reaction (aggregation); no reverse.",
)
def test_plup_joins_plamenu_custom_reaction(plup_mari, plamenu_user, cli):
    """Reverse of test_plamenu_joins_plup_custom_reaction: mari joins (+1) an
    existing Plamenu-*local* custom-emoji reaction. The author reacts with the
    bare local shortcode (seeding the emote into Pleroma via the EmojiReact's
    Emoji tag); mari then joins by the qualified `shortcode@plamenu-domain`, and
    the join aggregates into count 2 on Plamenu.

    Pleroma's PUT /reactions/{name} is strict about qualified names — it 400s an
    unknown or not-yet-ingested emoji — so the join is guarded with
    `pytest.xfail` (the suite's convention for an upstream quirk)."""
    plamenu_api = plamenu.login(plamenu_user)
    shortcode = unique("plamote")

    with step(f"create a local plamenu emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("mari follows the author (to receive the reaction join)"):
        account = plup_mari.resolve_account(plamenu_user.acct)
        assert account, "upstream Pleroma could not resolve the Plamenu account"
        plup_mari.follow(account["id"])
        wait_for(
            lambda: plup_mari.relationship(account["id"])["following"],
            desc="mari's follow of the author to be accepted",
        )

    display_name = f"{shortcode}@{config.PLAMENU_DOMAIN}"

    with step(
        "author posts and reacts with the BARE local emoji (seeds it into Pleroma)"
    ):
        status = plamenu_api.post_status("join my local reaction")
        plamenu_api.react(status["id"], shortcode)
        resolved = wait_for(
            lambda: plup_mari.resolve_status(status["uri"]),
            desc="upstream Pleroma to resolve the author's post",
        )
        wait_for(
            lambda: any(
                display_name in r["name"]
                for r in plup_mari.emoji_reactions(resolved["id"])
            ),
            desc=f"the {display_name} reaction (seeded from the EmojiReact) to appear on upstream Pleroma",
        )

    with step("mari JOINS the reaction by the qualified name"):
        try:
            plup_mari.react(resolved["id"], display_name)
        except ApiError as exc:
            pytest.xfail(
                f"upstream Pleroma remote-emoji join quirk (400 on qualified name): {exc}"
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
    direction="outbound", reverse_of="test_plup_custom_emote_reaction_on_plamenu"
)
def test_plamenu_custom_emote_reaction_on_plup(plup_mari, plamenu_user, cli):
    """Outbound `EmojiReact` with an `Emoji` tag — a reaction using a *local*
    Plamenu custom emoji reaches upstream Pleroma with its image, shown there as
    `shortcode@plamenu-domain`, and `Undo(EmojiReact)` retracts it."""
    plamenu_api = plamenu.login(plamenu_user)
    shortcode = unique("plamote")

    with step(f"create local emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("mari posts; plamenu resolves and reacts with the emoji"):
        plup_status = plup_mari.post_status("custom emote me from plamenu")
        resolved = plamenu_api.resolve_status(plup_status["uri"])
        assert resolved, f"Plamenu could not resolve {plup_status['uri']}"
        reacted = plamenu_api.react(resolved["id"], shortcode)
        groups = (reacted.get("pleroma") or {}).get("emoji_reactions", [])
        mine = next((r for r in groups if r["name"] == shortcode), None)
        assert mine and mine["me"] and mine["url"], (
            f"local reaction group must carry me + url: {groups}"
        )

    display_name = f"{shortcode}@{config.PLAMENU_DOMAIN}"

    with step("the emote reaction shows on upstream Pleroma with its image"):

        def plup_group():
            return next(
                (
                    r
                    for r in plup_mari.emoji_reactions(plup_status["id"])
                    if r["name"] == display_name
                ),
                None,
            )

        group = wait_for(
            lambda: plup_group(),
            desc=f"the {display_name} reaction to appear on upstream Pleroma",
        )
        assert group["count"] == 1, group
        assert group.get("url"), f"upstream Pleroma lost the emote image URL: {group}"

    with step("plamenu unreacts; Undo(EmojiReact) retracts it on upstream Pleroma"):
        plamenu_api.unreact(resolved["id"], shortcode)
        wait_for(
            lambda: plup_group() is None,
            desc=f"the {display_name} reaction to disappear from upstream Pleroma",
        )


@pytest.mark.federation(direction="both")
def test_reaction_counts_aggregate_across_instances_plup(plup_mari, plamenu_user, cli):
    """One reaction group merging reactors from two instances — a second local
    user and upstream Pleroma's mari both land in the same 🎉 group (count 2)."""
    author_api = plamenu.login(plamenu_user)

    with step("plamenu author posts; a second local user reacts 🎉"):
        status = author_api.post_status("aggregate reactions on me")
        other = plamenu.User()
        cli.account_add(other)
        other_api = plamenu.login(other)
        other_api.react(status["id"], "🎉")

    with step("mari reacts 🎉 to the same status"):
        resolved = plup_mari.resolve_status(status["uri"])
        assert resolved, f"upstream Pleroma could not resolve {status['uri']}"
        plup_mari.react(resolved["id"], "🎉")

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
        expected = sorted([other.username, f"{plup.MARI_NICK}@{config.PLUP_DOMAIN}"])
        assert accts == expected, f"reactor accounts {accts} != {expected}"
        assert group["me"] is False, "the author did not react; me must be false"
        log(f"aggregated group: count={group['count']} accounts={accts}")

    with step("the me flag is per-viewer: the local reactor sees me=true"):
        groups = other_api.emoji_reactions(status["id"])
        mine = next((r for r in groups if r["name"] == "🎉"), None)
        assert mine and mine["me"] is True, groups
