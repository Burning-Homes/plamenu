"""Differential response-shape parity against a live Mastodon.

3rd-party clients are coded to Mastodon's *exact* response shapes. Each test
here runs the identical operation against both servers and diffs the response
skeletons (`plamenu_e2e/shapes.py`), so any field/type/envelope drift fails
here instead of silently breaking real clients. See
`test_account_collections.py::test_collection_response_shapes_match_mastodon`
for the collections surface; this module covers the shared entities embedded
everywhere (accounts, statuses, …).
"""

import struct
import zlib

import pytest
from plamenu_e2e import config
from plamenu_e2e.shapes import assert_same_shape
from plamenu_e2e.steps import log, step, wait_for

# Plamenu additively emits the Pleroma/Akkoma emoji-reaction chips that Mastodon
# has no equivalent for: a top-level `emoji_reactions` array and a `pleroma`
# object mirroring it (real Akkoma emits both), plus the content-universe
# fields (`title`/`object_type`/`external_url`/`event` — Mastodon has no
# Status.title at all) and the group fields (`group_post`, `groups` with
# the attributing community Account entities,
# `downvotes_count`, `downvoted` — Lemmy-style scoring Mastodon lacks — plus
# `group_locked`, the moderator thread lock). They're intentional
# extensions clients ignore, so parity checks tolerate them rather than flag.
PLEROMA_EXT = {
    "pleroma",
    "emoji_reactions",
    "title",
    "object_type",
    "external_url",
    "event",
    "group_post",
    "groups",
    "downvotes_count",
    "downvoted",
    "group_locked",
    "hls",
    # A PeerTube live broadcast's state (`waiting`/`live`/`ended`), which
    # Mastodon has no concept of: it is what tells a client whether the
    # attachment is playable right now or should render as a poster.
    "live",
    # The AP URI of a reply parent we never fetched, so the web client can show
    # an "unfetched parent" notice instead of orphaning the reply. Always
    # present, null when the parent is absent or resolved locally.
    "in_reply_to_uri",
    # Mini-app invitation cards are a Plamenu extension to Status; ordinary
    # statuses carry null and Mastodon does not expose this field.
    "webxdc_invitation",
}


def _png(w=2, h=2):
    """A minimal valid PNG, so a media test needs no fixture files."""

    def chunk(tag, data):
        body = tag + data
        return (
            struct.pack(">I", len(data))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    raw = b"".join(b"\x00" + b"\xff\x00\x00" * w for _ in range(h))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


@pytest.mark.federation(direction="both")
def test_accounts_shapes_match_mastodon(alice, plamenu_api, plamenu_user):
    """`AccountSerializer` + `CredentialAccountSerializer` (incl. `source` and
    `role`) are embedded in nearly every other entity, so drift here ripples
    everywhere. Regression guard for the `moved` present-vs-absent,
    `source.attribution_domains` and `role.collection_limit` divergences."""
    with step("both accounts discoverable so each can resolve the other"):
        alice.update_profile(discoverable="true")
        plamenu_api.update_profile(discoverable="true")

    with step("verify_credentials (CredentialAccount: source + role)"):
        assert_same_shape(
            "verify_credentials",
            alice.get("/api/v1/accounts/verify_credentials"),
            plamenu_api.get("/api/v1/accounts/verify_credentials"),
            # Deliberate Plamenu extension: Mastodon keeps the
            # show-posting-app toggle web-UI-only; Plamenu surfaces it in
            # `source` so third-party clients can offer it too (8cf6c84).
            allow_extra={"show_application"},
        )
        log("CredentialAccount shape identical")

    with step("accounts/{id} — a local account viewed in full"):
        m_id = alice.get("/api/v1/accounts/verify_credentials")["id"]
        p_id = plamenu_api.get("/api/v1/accounts/verify_credentials")["id"]
        assert_same_shape(
            "accounts/{id} (local)",
            alice.get(f"/api/v1/accounts/{m_id}"),
            plamenu_api.get(f"/api/v1/accounts/{p_id}"),
        )
        log("local Account shape identical")

    with step("accounts/{id} — a remote account (each views the other side)"):
        remote_on_masto = alice.resolve_account(plamenu_user.acct)
        remote_on_plamenu = plamenu_api.resolve_account(config.ALICE)
        assert_same_shape("accounts/{id} (remote)", remote_on_masto, remote_on_plamenu)
        log("remote Account shape identical")

    with step("accounts/relationships"):
        rel_m = alice.get(
            "/api/v1/accounts/relationships", **{"id[]": remote_on_masto["id"]}
        )[0]
        rel_p = plamenu_api.get(
            "/api/v1/accounts/relationships", **{"id[]": remote_on_plamenu["id"]}
        )[0]
        assert_same_shape(
            "relationships",
            rel_m,
            rel_p,
            # Deliberate Plamenu extension: a follow can independently hide
            # replies, so the relationship exposes that local viewer state.
            allow_extra={"showing_replies"},
        )
        log("Relationship shape identical")


@pytest.mark.federation(direction="both")
def test_statuses_shapes_match_mastodon(alice, plamenu_api, plamenu_user):
    """`StatusSerializer` across its variants: a rich status (hashtag +
    mention + tags), the boost/reblog wrapper (a full Status in its own right —
    regression guard for the wrapper dropping `quote`/`quote_approval`), a poll
    status, and a media status. `pleroma` is an allowed additive extension."""
    with step("a rich status: hashtag + self-mention + tags"):
        m = alice.post_status(
            f"#e2etag hi @{alice.get('/api/v1/accounts/verify_credentials')['username']} here"
        )
        p = plamenu_api.post_status(f"#e2etag hi @{plamenu_user.username} here")
        assert_same_shape("status", m, p, allow_extra=PLEROMA_EXT)
        assert m["tags"] and p["tags"], "hashtag not parsed"
        assert m["mentions"] and p["mentions"], "mention not parsed"
        log("Status + tags[] + mentions[] shapes identical")

    with step("the reblog wrapper is a full Status (incl. quote fields)"):
        m_rb = alice.reblog(m["id"])
        p_rb = plamenu_api.reblog(p["id"])
        assert_same_shape("reblog wrapper", m_rb, p_rb, allow_extra=PLEROMA_EXT)
        assert "quote_approval" in m_rb and "quote_approval" in p_rb
        log("reblog wrapper (with quote/quote_approval) shapes identical")

    with step("a status carrying a poll (+ the Poll entity)"):
        m_poll = alice.post_poll("q?", ["a", "b"])
        p_poll = plamenu_api.post_poll("q?", ["a", "b"])
        assert_same_shape("status+poll", m_poll, p_poll, allow_extra=PLEROMA_EXT)
        assert_same_shape("poll", m_poll["poll"], p_poll["poll"])
        log("Status+poll and Poll entity shapes identical")

    with step("a media attachment and a status carrying it"):
        m_media = alice.upload_media(
            _png(), filename="x.png", mime="image/png", description="d"
        )
        p_media = plamenu_api.upload_media(
            _png(), filename="x.png", mime="image/png", description="d"
        )
        assert_same_shape("media_attachment", m_media, p_media)
        m_s = alice.post_with_media("with media", [m_media["id"]])
        p_s = plamenu_api.post_with_media("with media", [p_media["id"]])
        assert_same_shape("status+media", m_s, p_s, allow_extra=PLEROMA_EXT)
        log("MediaAttachment + Status.media_attachments[] shapes identical")


def _notif(api, kind):
    return lambda: next((n for n in api.notifications() if n["type"] == kind), None)


@pytest.mark.federation(direction="both")
def test_notifications_shapes_match_mastodon(alice, plamenu_api, plamenu_user, marker):
    """`NotificationSerializer` across its conditional attachments: `follow`
    (statusless — must NOT carry `status`), `favourite` (status-bearing), and
    `added_to_collection` (must carry the full `collection` incl. `items`, the
    data a client needs to render the 'remove me from this collection'
    action). Generated bidirectionally over real federation."""
    alice.update_profile(discoverable="true")
    plamenu_api.update_profile(discoverable="true")
    p_on_m = alice.resolve_account(plamenu_user.acct)
    a_on_p = plamenu_api.resolve_account(config.ALICE)

    with step("follow notification omits `status` (statusless type)"):
        alice.follow(p_on_m["id"])
        plamenu_api.follow(a_on_p["id"])
        m = wait_for(_notif(alice, "follow"), desc="mastodon follow notif")
        p = wait_for(_notif(plamenu_api, "follow"), desc="plamenu follow notif")
        assert "status" not in p, f"follow notif must not carry `status`: {p}"
        assert_same_shape("follow notif", m, p, allow_extra=PLEROMA_EXT)
        log("follow notification shape identical (no status key)")

    with step("favourite notification carries a `status`"):
        ap = alice.post_status(f"m fav src {marker}")
        pp = plamenu_api.post_status(f"p fav src {marker}")
        ap_on_p = wait_for(
            lambda: plamenu_api.resolve_status(ap["uri"]),
            desc="plamenu resolves alice post",
        )
        pp_on_m = wait_for(
            lambda: alice.resolve_status(pp["uri"]),
            desc="mastodon resolves plamenu post",
        )
        alice.favourite(pp_on_m["id"])
        plamenu_api.favourite(ap_on_p["id"])
        m = wait_for(_notif(alice, "favourite"), desc="mastodon favourite notif")
        p = wait_for(_notif(plamenu_api, "favourite"), desc="plamenu favourite notif")
        assert_same_shape("favourite notif", m, p, allow_extra=PLEROMA_EXT)
        log("favourite notification shape identical (with status)")

    with step("added_to_collection carries `collection` with `items`"):
        # The Mastodon instance is persistent and caps collections per account;
        # prune this test's leftovers so repeat runs don't 422 (too_many).
        alice_id = alice.get("/api/v1/accounts/verify_credentials")["id"]
        listing = alice.get(f"/api/v1/accounts/{alice_id}/collections")
        for old in listing.get(
            "collections", listing if isinstance(listing, list) else []
        ):
            if old["name"].startswith("mc "):
                alice.http.delete(
                    f"{alice.base_url}/api/v1/collections/{old['id']}", timeout=30
                )
        plamenu_api.post(
            "/api/v1/collections",
            name=f"pc {marker}",
            description="d",
            discoverable="true",
            **{"account_ids[]": a_on_p["id"]},
        )
        alice.post(
            "/api/v1/collections",
            name=f"mc {marker}",
            description="d",
            sensitive="false",
            discoverable="true",
            **{"account_ids[]": p_on_m["id"]},
        )
        m = wait_for(
            _notif(alice, "added_to_collection"), desc="mastodon collection notif"
        )
        p = wait_for(
            _notif(plamenu_api, "added_to_collection"), desc="plamenu collection notif"
        )
        assert_same_shape("added_to_collection notif", m, p, allow_extra=PLEROMA_EXT)
        assert p["collection"]["items"], (
            f"collection notif must carry items (the revoke-button data): {p}"
        )
        log("added_to_collection shape identical (collection + items present)")

    with step("v2 grouped: the collection group carries `collection` with items"):

        def coll_group(api):
            groups = api.get("/api/v2/notifications")["notification_groups"]
            return next((g for g in groups if g["type"] == "added_to_collection"), None)

        m_g = wait_for(lambda: coll_group(alice), desc="mastodon v2 collection group")
        p_g = wait_for(
            lambda: coll_group(plamenu_api), desc="plamenu v2 collection group"
        )
        # Diff the group entity itself (the top-level accounts/statuses arrays
        # differ in account locality on a persistent Mastodon, so scope to the
        # group).
        assert_same_shape("v2 collection group", m_g, p_g, allow_extra=PLEROMA_EXT)
        assert p_g["collection"]["items"], (
            f"v2 group must carry the collection's items: {p_g}"
        )
        log("v2 grouped collection group shape identical (collection + items)")


@pytest.mark.federation(direction="both")
def test_timelines_context_search_shapes_match_mastodon(alice, plamenu_api, marker):
    """Timeline / thread / search *envelopes* (the Status element shape itself
    is covered by the statuses test): `context` ({ancestors, descendants}),
    the bare public-timeline array, and `/api/v2/search`
    ({accounts, statuses, hashtags, collections}) incl. the Tag entity."""
    with step("GET /statuses/{id}/context ({ancestors, descendants})"):
        mp = alice.post_status(f"ctx parent {marker}")
        alice.post_status(f"ctx reply {marker}", in_reply_to_id=mp["id"])
        pp = plamenu_api.post_status(f"ctx parent {marker}")
        plamenu_api.post_status(f"ctx reply {marker}", in_reply_to_id=pp["id"])
        assert_same_shape(
            "context",
            alice.context(mp["id"]),
            plamenu_api.context(pp["id"]),
            allow_extra=PLEROMA_EXT,
        )
        log("context envelope + descendant Status shape identical")

    with step("GET /timelines/public (bare array of Status)"):
        assert_same_shape(
            "public timeline",
            alice.public_timeline(local=True, limit=1)[:1],
            plamenu_api.public_timeline(local=True, limit=1)[:1],
            allow_extra=PLEROMA_EXT,
        )
        log("public timeline element shape identical")

    with step("GET /api/v2/search ({accounts, statuses, hashtags, collections})"):
        alice.post_status(f"#e2etl{marker} tagged")
        plamenu_api.post_status(f"#e2etl{marker} tagged")
        m_s = alice.search(f"e2etl{marker}", resolve=False)
        p_s = plamenu_api.search(f"e2etl{marker}", resolve=False)
        assert set(m_s) == set(p_s), f"search top keys differ: {set(m_s)} vs {set(p_s)}"
        assert_same_shape("search hashtags[]", m_s["hashtags"][:1], p_s["hashtags"][:1])
        log("search envelope + Tag entity shape identical")


@pytest.mark.federation(direction="both")
def test_conversation_shape_matches_mastodon(alice, plamenu_api, plamenu_user, marker):
    """The `Conversation` entity ({id, unread, accounts, last_status}), built
    from a real cross-instance DM (a mutual follow first, so the DM reaches
    `/conversations` rather than being stranger-filtered)."""
    p_on_m = alice.resolve_account(plamenu_user.acct)
    a_on_p = plamenu_api.resolve_account(config.ALICE)
    with step("establish a mutual follow"):
        alice.follow(p_on_m["id"])
        plamenu_api.follow(a_on_p["id"])
        wait_for(
            lambda: plamenu_api.relationship(a_on_p["id"]).get("followed_by"),
            desc="plamenu sees alice's follow",
        )
        wait_for(
            lambda: alice.relationship(p_on_m["id"]).get("followed_by"),
            desc="mastodon sees plamenu's follow",
        )

    with step("each sends a direct message, then read /conversations"):
        alice.post_status(
            f"@{plamenu_user.username}@{config.PLAMENU_DOMAIN} dm {marker}",
            visibility="direct",
        )
        plamenu_api.post_status(f"@{config.ALICE} dm {marker}", visibility="direct")
        m_conv = wait_for(
            lambda: alice.conversation_containing(marker), desc="mastodon conversation"
        )
        p_conv = wait_for(
            lambda: plamenu_api.conversation_containing(marker),
            desc="plamenu conversation",
        )
        # Compare the Conversation envelope only: each side's `last_status`
        # author has different locality (own DM local on one side, the peer's
        # DM remote on the other), so a deep status diff would compare
        # apples-to-oranges — the Status/Account shapes are covered by the
        # statuses/accounts tests. Here we assert the envelope + member types.
        assert set(m_conv) == set(p_conv), (
            f"Conversation envelope keys differ: {sorted(m_conv)} vs {sorted(p_conv)}"
        )
        assert isinstance(p_conv["accounts"], list) and p_conv["accounts"], p_conv
        assert isinstance(p_conv["last_status"], dict) and "id" in p_conv["last_status"]
        assert isinstance(p_conv["unread"], bool) and isinstance(p_conv["id"], str)
        log("Conversation envelope shape identical (id, unread, accounts, last_status)")
