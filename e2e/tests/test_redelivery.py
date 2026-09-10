"""Redelivered (replayed) activities must be no-ops — the Pleroma incident.

In 2026-07 a Pleroma peer re-sent roughly a full day of already-processed
activities. Every replayed Like/Announce/Create/EmojiReact re-created its
notification (streamed and web-pushed by the insert trigger) and re-published
day-old posts to live "as they arrive" timelines. These tests replay real,
signed activities straight into Plamenu's inbox (see plamenu_e2e/redeliver.py)
and assert the replay changes nothing — while genuinely new information
travelling over the *same* activity types (successive edits of the same post)
still lands, so deduplication never eats an edit.
"""

import json
import ssl
import time

import pytest
from plamenu_e2e import config, pleroma, redeliver, unique
from plamenu_e2e.steps import log, step, wait_for
from websockets.sync.client import connect

BOB_ACTOR = f"{config.PLEROMA_URL}/users/{pleroma.BOB_NICK}"


def _open_stream(token: str):
    """A websocket on Plamenu's `user` stream (TLS trust is not under test)."""
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    base = config.PLAMENU_URL.replace("https://", "wss://")
    return connect(
        f"{base}/api/v1/streaming?access_token={token}&stream=user",
        ssl=ctx,
        open_timeout=30,
    )


def _events_until_update_containing(ws, text: str, *, timeout: float = 90.0) -> list:
    """Every streamed event up to (and excluding) the `update` whose payload
    contains `text` — the sentinel technique: anything a replay wrongly
    emitted must arrive before a post made after it."""
    seen = []
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"sentinel update containing {text!r} never arrived"
        message = json.loads(ws.recv(timeout=remaining))
        log(f"<< stream: {message.get('event')}")
        payload = message.get("payload", "")
        if message.get("event") == "update" and text in payload:
            return seen
        seen.append(message)


def _bob_key() -> str:
    return redeliver.pleroma_private_key_pem(pleroma.BOB_NICK)


def _notification_ids(api) -> list:
    return sorted(n["id"] for n in api.notifications())


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound replay-dedup: a replayed backlog of activities is ignored on re-delivery; a receive-side idempotency guarantee with no outbound counterpart.",
)
def test_replayed_backlog_is_ignored(
    pleroma_bob, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: replaying a peer's backlog — Like, Announce (fresh activity
    id), Create-with-mention, EmojiReact, all signed by the real actor —
    produces no new notifications, no duplicate rows, no streaming events,
    and no timeline resurfacing."""
    with step(f"{plamenu_user.username} follows bob@{config.PLEROMA_DOMAIN}"):
        cli.follow(plamenu_user.username, f"bob@{config.PLEROMA_DOMAIN}")
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    bait_marker = unique("bait")
    with step("bob replies to, favourites, boosts and reacts to a Plamenu post"):
        bait = plamenu_api.post_status(f"interaction bait {bait_marker}")
        found = pleroma_bob.search(bait["uri"], resolve=True, type="statuses")[
            "statuses"
        ]
        assert found, "Pleroma could not resolve the bait post"
        # A reply carries a Mention tag of the parent author (a bare remote
        # @mention would not — this Pleroma leaves those as plain text).
        bob_reply = pleroma_bob.post_status(
            f"backlog reply {marker}",
            in_reply_to_id=found[0]["id"],
            visibility="public",
        )
        note_uri = bob_reply["uri"]
        pleroma_bob.favourite(found[0]["id"])
        pleroma_bob.reblog(found[0]["id"])
        pleroma_bob.react(found[0]["id"], "🔥")
        for kind in ("mention", "favourite", "reblog", "pleroma:emoji_reaction"):
            wait_for(
                lambda k=kind: plamenu_api.notifications_from(
                    f"bob@{config.PLEROMA_DOMAIN}", k
                ),
                desc=f"the {kind} notification",
            )
        wait_for(
            lambda: plamenu_api.home_status_containing(marker),
            desc="bob's reply to reach the home timeline",
        )

    before = _notification_ids(plamenu_api)
    key = _bob_key()
    token = plamenu_api.http.headers["Authorization"].removeprefix("Bearer ")
    sentinel = unique("sentinel")

    with _open_stream(token) as ws:
        with step("replay the whole backlog, signed as the real bob"):
            note = redeliver.fetch_ap(note_uri)
            replays = [
                redeliver.create_envelope(note, actor_uri=BOB_ACTOR),
                {
                    "@context": redeliver.AS2_CONTEXT,
                    "id": f"{BOB_ACTOR}#replays/like",
                    "type": "Like",
                    "actor": BOB_ACTOR,
                    "object": bait["uri"],
                },
                {
                    "@context": redeliver.AS2_CONTEXT,
                    "id": f"{BOB_ACTOR}#replays/announce",
                    "type": "Announce",
                    "actor": BOB_ACTOR,
                    "to": [redeliver.PUBLIC],
                    "object": bait["uri"],
                },
                {
                    "@context": redeliver.AS2_CONTEXT,
                    "id": f"{BOB_ACTOR}#replays/react",
                    "type": "EmojiReact",
                    "actor": BOB_ACTOR,
                    "content": "🔥",
                    "object": bait["uri"],
                },
            ]
            for activity in replays:
                code = redeliver.deliver(
                    activity, actor_uri=BOB_ACTOR, private_key_pem=key
                )
                assert code == 202, f"inbox answered {code} for {activity['type']}"

        with step("nothing leaked to the live stream before a sentinel post"):
            pleroma_bob.post_status(f"sentinel {sentinel}", visibility="public")
            leaked = _events_until_update_containing(ws, sentinel)
            notifications = [m for m in leaked if m.get("event") == "notification"]
            resurfaced = [
                m
                for m in leaked
                if m.get("event") == "update" and marker in m.get("payload", "")
            ]
            assert not notifications, f"replay leaked notifications: {notifications}"
            assert not resurfaced, "replayed Create resurfaced the old post live"

    with step("notifications and rows are unchanged"):
        assert _notification_ids(plamenu_api) == before, (
            "the replay must not add or remove notifications"
        )
        boosts = db.conn.execute(
            "SELECT count(*) FROM statuses boost"
            " JOIN statuses orig ON orig.id = boost.reblog_of_id"
            " WHERE orig.content LIKE %s",
            (f"%{bait_marker}%",),
        ).fetchone()[0]
        assert boosts == 1, f"expected exactly one boost row, got {boosts}"
        favourites = db.conn.execute(
            "SELECT count(*) FROM favourites f"
            " JOIN statuses s ON s.id = f.status_id WHERE s.content LIKE %s",
            (f"%{bait_marker}%",),
        ).fetchone()[0]
        assert favourites == 1, f"expected exactly one favourite row, got {favourites}"

    with step("the old post sits in the home timeline exactly once"):
        entries = [
            s
            for s in plamenu_api.home_timeline(limit=40)
            if not s.get("reblog") and marker in s["content"]
        ]
        assert len(entries) == 1, f"expected the post once, got {len(entries)}"


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound replay-dedup refinement: de-duplication must not drop a genuine later edit; receive-side only, no outbound counterpart.",
)
def test_dedup_never_eats_edits(
    pleroma_bob, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: two successive genuine edits of the same post both land —
    same event type, same post, twice — while replaying the original Create
    or a stale Update in between never rolls the content back and never
    re-notifies the mention."""
    with step(f"{plamenu_user.username} follows bob@{config.PLEROMA_DOMAIN}"):
        cli.follow(plamenu_user.username, f"bob@{config.PLEROMA_DOMAIN}")
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    with step("bob replies to the user with v1 (a reply carries the mention)"):
        parent = plamenu_api.post_status(f"edit thread root {marker}")
        found = pleroma_bob.search(parent["uri"], resolve=True, type="statuses")[
            "statuses"
        ]
        assert found, "Pleroma could not resolve the parent post"
        bob_post = pleroma_bob.post_status(
            f"redelivery v1 {marker}",
            in_reply_to_id=found[0]["id"],
            visibility="public",
        )
        note_uri = bob_post["uri"]
        wait_for(
            lambda: plamenu_api.home_status_containing(f"v1 {marker}"),
            desc="v1 to reach the home timeline",
        )
    v1_note = redeliver.fetch_ap(note_uri)

    with step("bob edits to v2; the edit lands (first Update applies)"):
        pleroma_bob.edit_status(bob_post["id"], f"redelivery v2 {marker}")
        wait_for(
            lambda: plamenu_api.home_status_containing(f"v2 {marker}"),
            desc="the v2 edit to apply on Plamenu",
        )
    v2_note = redeliver.fetch_ap(note_uri)
    assert v2_note.get("updated"), "Pleroma should stamp `updated` on the edit"

    key = _bob_key()
    with step("replaying the original Create must not roll v2 back"):
        code = redeliver.deliver(
            redeliver.create_envelope(v1_note, actor_uri=BOB_ACTOR),
            actor_uri=BOB_ACTOR,
            private_key_pem=key,
        )
        assert code == 202
        assert plamenu_api.home_status_containing(f"v2 {marker}"), (
            "a replayed Create rolled the post back to v1"
        )

    with step("bob edits to v3; the second edit also lands (dedup must not overreact)"):
        pleroma_bob.edit_status(bob_post["id"], f"redelivery v3 {marker}")
        wait_for(
            lambda: plamenu_api.home_status_containing(f"v3 {marker}"),
            desc="the v3 edit to apply on Plamenu",
        )

    with step("replaying the stale v2 Update must not roll v3 back"):
        code = redeliver.deliver(
            redeliver.update_envelope(v2_note, actor_uri=BOB_ACTOR),
            actor_uri=BOB_ACTOR,
            private_key_pem=key,
        )
        assert code == 202
        assert plamenu_api.home_status_containing(f"v3 {marker}"), (
            "a replayed stale Update rolled the post back to v2"
        )

    with step("through it all, exactly one mention notification"):
        mentions = plamenu_api.notifications_from(
            f"bob@{config.PLEROMA_DOMAIN}", "mention"
        )
        assert len(mentions) == 1, f"expected one mention, got {len(mentions)}"
