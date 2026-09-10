"""Reaction dialects against Sharkey (cross-software track).

Sharkey federates *every* reaction — including its default one — as a `Like`
carrying `_misskey_reaction` (mirrored into `content`), with a tagged `Emoji`
for custom emotes, and withdraws it with an `Undo` embedding that Like. These
tests drive the real wire both ways: inbound Sharkey reactions must land as
Plamenu emoji reactions (never plain favourites), and Plamenu's outbound
`EmojiReact`/`Undo(EmojiReact)` must register on Sharkey's reaction tally.
"""

import base64
import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import config, plamenu, unique
from plamenu_e2e.steps import log, step, wait_for

# A 1x1 transparent PNG — small enough for every emoji size limit.
PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhf"
    "DwAChwGA60e6kgAAAABJRU5ErkJggg=="
)

# The suite-managed custom emoji on the Sharkey side: a fixed shortcode, so
# re-runs are idempotent (ensure_custom_emoji tolerates an existing one).
SHARKEY_EMOTE = "e2eshonk"


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_reaction_surfaces_on_sharkey"
)
def test_sharkey_reaction_arrives_and_undoes(sharkey_carol, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("plamenu user posts a public status"):
        status = plamenu_api.post_status(
            "react to me from sharkey", visibility="public"
        )
        status_uri = status["uri"]
        log(f"status {status['id']} at {status_uri}")

    with step("sharkey carol resolves the post and reacts with 🍮"):
        shown = sharkey_carol.resolve(status_uri)
        assert shown["type"] == "Note", shown
        note_id = shown["object"]["id"]
        sharkey_carol.react(note_id, "🍮")

    with step("the Like-dialect reaction lands as an emoji reaction"):

        def reactions():
            return (
                plamenu_api.get_status(status["id"])
                .get("pleroma", {})
                .get("emoji_reactions", [])
            )

        wait_for(
            lambda: any(r["name"] == "🍮" and r["count"] >= 1 for r in reactions()),
            desc="🍮 reaction to appear in pleroma.emoji_reactions",
        )
        log(f"reactions now: {reactions()}")

    with step("it is a reaction, not a favourite"):
        shown = plamenu_api.get_status(status["id"])
        assert shown["favourites_count"] == 0, shown["favourites_count"]
        notifs = plamenu_api.get("/api/v1/notifications")
        kinds = [n["type"] for n in notifs]
        assert "pleroma:emoji_reaction" in kinds, kinds
        assert "favourite" not in kinds, kinds

    with step("carol unreacts and Sharkey's Undo(Like) retracts it"):
        sharkey_carol.unreact(note_id)
        wait_for(
            lambda: not any(r["name"] == "🍮" for r in reactions()),
            desc="🍮 reaction to disappear after Undo",
        )


@pytest.mark.federation(
    direction="inbound",
    reverse_of="test_plamenu_custom_emoji_reaction_surfaces_on_sharkey",
)
def test_sharkey_custom_emoji_reaction(sharkey_carol, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step(f"sharkey has the custom emoji :{SHARKEY_EMOTE}:"):
        sharkey_carol.ensure_custom_emoji(SHARKEY_EMOTE, base64.b64decode(PNG_BASE64))

    with step("plamenu user posts and carol reacts with the custom emote"):
        status = plamenu_api.post_status("custom-react to me", visibility="public")
        shown = sharkey_carol.resolve(status["uri"])
        note_id = shown["object"]["id"]
        sharkey_carol.react(note_id, f":{SHARKEY_EMOTE}:")

    with step("the reaction shows qualified, with its federated image"):

        def reactions():
            return (
                plamenu_api.get_status(status["id"])
                .get("pleroma", {})
                .get("emoji_reactions", [])
            )

        qualified = f"{SHARKEY_EMOTE}@sharkey.local"
        wait_for(
            lambda: any(r["name"] == qualified for r in reactions()),
            desc=f"custom reaction {qualified} to appear",
        )
        chip = next(r for r in reactions() if r["name"] == qualified)
        assert chip.get("url"), f"custom emoji lost its image: {chip}"


@pytest.mark.federation(
    direction="outbound", reverse_of="test_sharkey_reaction_arrives_and_undoes"
)
def test_plamenu_reaction_surfaces_on_sharkey(sharkey_carol, plamenu_user, db):
    plamenu_api = plamenu.login(plamenu_user)

    with step("sharkey carol posts a note"):
        note = sharkey_carol.post_note(f"react to me from plamenu {unique('mark')}")
        note_uri = f"{sharkey_carol.base_url}/notes/{note['id']}"
        log(f"note {note['id']} at {note_uri}")

    with step("plamenu resolves the note and reacts with 🔥"):
        resolved = plamenu_api.search(note_uri, resolve=True, type="statuses")
        found = resolved["statuses"]
        assert found, f"Plamenu could not resolve {note_uri}"
        plamenu_status_id = found[0]["id"]
        plamenu_api.react(plamenu_status_id, "🔥")

    with step("the EmojiReact registers on Sharkey's tally"):
        wait_for(
            lambda: sharkey_carol.note_reactions(note["id"]).get("🔥", 0) >= 1,
            desc="🔥 to appear in the Sharkey note's reactions",
        )
        log(f"sharkey tally: {sharkey_carol.note_reactions(note['id'])}")

    with step("plamenu unreacts and the Undo clears it"):
        plamenu_api.unreact(plamenu_status_id, "🔥")
        wait_for(
            lambda: sharkey_carol.note_reactions(note["id"]).get("🔥", 0) == 0,
            desc="🔥 to disappear from the Sharkey tally",
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_sharkey_custom_emoji_reaction"
)
def test_plamenu_custom_emoji_reaction_surfaces_on_sharkey(
    sharkey_carol, plamenu_user, cli
):
    """Reverse of test_sharkey_custom_emoji_reaction: Plamenu reacts with a
    *local* custom emoji; the outbound `EmojiReact` carries an `Emoji` tag with
    the emote's image, and Sharkey keys it as the qualified `:shortcode@host:`
    reaction (colon-wrapped, like every remote custom emote on Misskey)."""
    plamenu_api = plamenu.login(plamenu_user)
    shortcode = unique("blobpla")

    with step(f"create a local plamenu custom emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("carol posts a note; plamenu resolves it"):
        note = sharkey_carol.post_note(f"custom-react to me {unique('mark')}")
        note_uri = f"{sharkey_carol.base_url}/notes/{note['id']}"
        resolved = wait_for(
            lambda: plamenu_api.resolve_status(note_uri),
            desc="Plamenu to resolve carol's note",
        )
        status_id = resolved["id"]

    with step("plamenu reacts with the local custom emoji"):
        plamenu_api.react(status_id, shortcode)

    qualified = f":{shortcode}@{config.PLAMENU_DOMAIN}:"

    with step("the reaction shows qualified on Sharkey's tally"):
        wait_for(
            lambda: qualified in sharkey_carol.note_reactions(note["id"]),
            desc=f"{qualified} to appear in the Sharkey note reactions",
        )

    with step("plamenu unreacts and the Undo clears it"):
        plamenu_api.unreact(status_id, shortcode)
        wait_for(
            lambda: qualified not in sharkey_carol.note_reactions(note["id"]),
            desc="the custom reaction to disappear after Undo",
        )
