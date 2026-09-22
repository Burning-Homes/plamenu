"""Plamenu against Plamenu: what a post and a profile carry with them.

The companion to `test_plamenu2_federation.py`. That module is about the
verbs; this one is about the payload — attachments, profile metadata, pins,
long-form articles, custom emoji, quotes, private conversations and featured
collections. Several of these are dialects no other peer in the fleet
implements on both ends, so a round trip here is the only place both halves
are checked at once.

Direction is `both` throughout: identical software on either side means an
"inbound" and an "outbound" test of the same round trip would assert the same
code twice under two names.
"""

import base64
import tempfile
from pathlib import Path

import pytest
import requests
from plamenu_e2e import interop, media, unique
from plamenu_e2e.interop import as_a_reader_rewrites_it
from plamenu_e2e.steps import log, step, wait_for

# A 1x1 transparent PNG, small enough for every emoji size limit.
EMOJI_PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhf"
    "DwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


def emoji_file() -> str:
    """Write the emoji PNG somewhere the CLI can read it; returns the path."""
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as handle:
        handle.write(base64.b64decode(EMOJI_PNG_BASE64))
        return handle.name


@pytest.mark.federation(direction="both")
def test_image_attachments_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """An image with everything an image can carry.

    Covers: the `Document` attachment on the wire, the reader caching the file
    onto its own `/media/` route (never hot-linking the author's server), and
    the alt text, focal point, blurhash and dimensions arriving intact — then
    the same the other way."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user posts an image with alt text and a focal point"):
        upload = plamenu_api.upload_media(
            media.make_png(96, 64, (10, 120, 200)),
            filename="pic.png",
            mime="image/png",
            description=f"a rectangle {marker}",
            focus="0.4,-0.3",
        )
        posted = plamenu_api.post_with_media(f"look at this {marker}", [upload["id"]])
        mine = posted["media_attachments"][0]
        assert mine["blurhash"], mine

    with step("the reader caches the file and keeps every attribute"):
        theirs = interop.delivered(plamenu2_api, marker)
        attachment = wait_for(
            lambda: media.cached_attachment(
                plamenu2_api,
                theirs["id"],
                kind="image",
                require_blurhash=True,
                base_url=plamenu2.url,
            ),
            desc="the peer to cache the image and backfill its metadata",
        )
        assert attachment["description"] == mine["description"], attachment
        assert attachment["blurhash"] == mine["blurhash"], attachment
        assert attachment["meta"]["focus"] == mine["meta"]["focus"], attachment
        assert attachment["meta"]["original"]["width"] == 96, attachment
        assert attachment["meta"]["original"]["height"] == 64, attachment
        assert plamenu_user.domain not in attachment["url"], (
            f"the reader must serve its own copy, not hot-link: {attachment['url']}"
        )

    with step("and the cached file is really there"):
        response = requests.get(attachment["url"], verify=False, timeout=30)
        assert response.ok, response.status_code
        assert response.headers["content-type"].startswith("image/"), response.headers

    with step("the reverse direction: an image published by the peer"):
        their_upload = plamenu2_api.upload_media(
            media.make_png(32, 32, (200, 30, 30)),
            filename="dot.png",
            mime="image/png",
            description=f"a square {marker}",
        )
        plamenu2_api.post_with_media(f"and this {marker}", [their_upload["id"]])
        ours = interop.delivered(plamenu_api, f"and this {marker}")
        cached = wait_for(
            lambda: media.cached_attachment(
                plamenu_api, ours["id"], kind="image", require_blurhash=True
            ),
            desc="us to cache the peer's image",
        )
        assert cached["description"] == f"a square {marker}", cached


def test_article_inline_images_remain_compatibility_attachments(
    plamenu2, plamenu2_api, marker
):
    """AP11 on a freshly-built live Plamenu: inline Article media renders
    in-place while every image remains a normal client-API and ActivityPub
    attachment. A second image stays in the first-party attachment gallery."""

    with step("the author publishes an Article with inline and attached images"):
        diagram = plamenu2_api.upload_media(
            media.make_png(80, 50, (20, 130, 220)),
            filename="diagram.png",
            mime="image/png",
            description=f"flow diagram {marker}",
        )
        appendix = plamenu2_api.upload_media(
            media.make_png(40, 60, (220, 100, 20)),
            filename="appendix.png",
            mime="image/png",
            description=f"appendix {marker}",
        )
        posted = plamenu2_api.post_status(
            f"Before [[media:{diagram['id']}]] after {marker}",
            post_kind="article",
            title=f"Illustrated {marker}",
            **{"media_ids[]": [diagram["id"], appendix["id"]]},
        )
        assert len(posted["media_attachments"]) == 2, posted
        assert "status__inline-image" in posted["content"], posted["content"]
        assert f'data-media-id="{diagram["id"]}"' in posted["content"]

        activity = plamenu2_api.ap_get(posted["uri"].removeprefix(plamenu2.url))
        assert activity["type"] == "Article", activity
        assert len(activity["attachment"]) == 2, activity
        assert "status__inline-image" in activity["content"], activity["content"]

    with step("the built-in client places only the selected image inline"):
        page = requests.get(posted["url"], verify=False, timeout=30)
        assert page.ok, page.status_code
        assert page.text.count(f'data-media-id="{diagram["id"]}"') == 1, page.text
        assert f'aria-label="ALT: flow diagram {marker}"' in page.text, page.text
        assert f'aria-label="ALT: appendix {marker}"' in page.text, page.text
        assert page.text.count('class="media__alt-text" role="tooltip"') == 2, page.text
        appendix_url = next(
            item["url"]
            for item in posted["media_attachments"]
            if item["id"] == appendix["id"]
        )
        assert appendix_url in page.text, page.text

    with step("the client API keeps both compatibility attachments"):
        received = plamenu2_api.get(f"/api/v1/statuses/{posted['id']}")
        assert len(received["media_attachments"]) == 2, received
        descriptions = {item["description"] for item in received["media_attachments"]}
        assert descriptions == {f"flow diagram {marker}", f"appendix {marker}"}


@pytest.mark.federation(direction="both")
def test_profile_edits_reach_followers(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """`Update(Actor)` — the profile a follower's server keeps in sync.

    Covers: display name, bio, profile fields and the bot flag federating on a
    profile edit, the avatar being cached by the reader rather than hot-linked,
    and the same edit going the other way."""
    theirs, mine = interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user rewrites their profile"):
        updated = plamenu_api.update_profile(
            files={
                "avatar": ("avatar.png", media.tiny_png((10, 200, 90)), "image/png")
            },
            display_name=f"Renamed {marker}",
            note=f"a new bio {marker}",
            bot="true",
            **{
                "fields_attributes[0][name]": "Website",
                "fields_attributes[0][value]": "https://example.com/",
                "fields_attributes[1][name]": "Marker",
                "fields_attributes[1][value]": marker,
            },
        )
        assert updated["display_name"] == f"Renamed {marker}"
        assert [f["name"] for f in updated["fields"]] == ["Website", "Marker"]

    with step("the follower's server catches up with all of it"):
        refreshed = wait_for(
            lambda: (
                account
                if (account := plamenu2_api.account(mine["id"]))
                and account["display_name"] == f"Renamed {marker}"
                else None
            ),
            desc="the Update(Actor) to refresh the follower's copy",
        )
        assert marker in refreshed["note"], refreshed["note"]
        assert refreshed["bot"] is True, refreshed
        assert [
            (f["name"], as_a_reader_rewrites_it(f["value"]))
            for f in refreshed["fields"]
        ] == [
            ("Website", as_a_reader_rewrites_it(updated["fields"][0]["value"])),
            ("Marker", marker),
        ], refreshed["fields"]
        assert refreshed["avatar"].startswith(f"{plamenu2.url}/media/"), (
            f"the follower must cache the avatar, not hot-link it: "
            f"{refreshed['avatar']}"
        )

    with step("the reverse direction: the peer renames itself"):
        plamenu2_api.update_profile(display_name=f"Peer renamed {marker}")
        wait_for(
            lambda: (
                (plamenu_api.account(theirs["id"]) or {})["display_name"]
                == f"Peer renamed {marker}"
            ),
            desc="the peer's Update(Actor) to reach us",
        )


@pytest.mark.federation(direction="both")
def test_pins_and_featured_tags_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """`Add`/`Remove` against the featured collection, for both kinds of item.

    Covers: a pinned status and a featured hashtag showing up on the profile as
    the other instance sees it, and both disappearing again on the Remove."""
    theirs, _mine = interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the peer pins a post and features a tag"):
        posted = plamenu2_api.post_status(f"worth keeping {marker}")
        assert plamenu2_api.post(f"/api/v1/statuses/{posted['id']}/pin")["pinned"]
        featured = plamenu2_api.post("/api/v1/featured_tags", name=f"#{marker}")
        assert featured["name"] == marker

    with step("both show up on the profile as the other instance sees it"):
        wait_for(
            lambda: any(
                marker in status["content"]
                for status in plamenu_api.account_statuses(theirs["id"], pinned="true")
            ),
            desc="the Add(pin) to reach the follower",
        )
        wait_for(
            lambda: any(
                tag["name"] == marker
                for tag in plamenu_api.get(
                    f"/api/v1/accounts/{theirs['id']}/featured_tags"
                )
            ),
            desc="the Add(Hashtag) to reach the follower",
        )

    with step("unpinning and unfeaturing take them away again"):
        plamenu2_api.post(f"/api/v1/statuses/{posted['id']}/unpin")
        plamenu2_api.delete(f"/api/v1/featured_tags/{featured['id']}")
        wait_for(
            lambda: not plamenu_api.account_statuses(theirs["id"], pinned="true"),
            desc="the Remove(pin) to reach the follower",
        )
        wait_for(
            lambda: (
                not plamenu_api.get(f"/api/v1/accounts/{theirs['id']}/featured_tags")
            ),
            desc="the Remove(Hashtag) to reach the follower",
        )


@pytest.mark.federation(direction="both")
def test_long_form_articles_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """An `Article`, read by a server that knows what one is.

    Every existing long-form test is one-way, because no peer in the fleet can
    *author* an article — Mastodon truncates ours to a stub, the rest render it
    whole, and none of them can send one back. Here both ends can, so this is
    the only place the reader half is exercised: the title arrives as a title,
    the body arrives whole, and the heading the author baked into the body for
    the benefit of title-blind servers is stripped by one that isn't."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )
    body = (
        "This paragraph is longer than any title-plus-link stub could carry, "
        f"and this last sentence is what proves the body arrived: {marker}"
    )

    with step("the local user publishes a long-form post"):
        title = f"An essay {marker}"
        posted = plamenu_api.post_status(body, post_kind="article", title=title)
        assert posted["object_type"] == "Article", posted
        assert posted["title"] == title, posted

    with step("the reader gets the title as a title and the body whole"):
        theirs = interop.delivered(plamenu2_api, marker)
        assert theirs["object_type"] == "Article", theirs
        assert theirs["title"] == title, theirs
        assert marker in theirs["content"], theirs["content"]
        assert "longer than any title-plus-link stub" in theirs["content"]
        # The title travels twice — as `name`, and as a heading at the top of
        # the body for servers that ignore `name` (LONGFORM_DESIGN.md §2.1).
        # A reader that understands `name` strips that baked heading and folds
        # the title back in itself, so the headline appears exactly once and
        # the reader's rendering is the author's rendering.
        assert theirs["content"].count(title) == 1, (
            "a title-aware reader must strip the duplicated heading: "
            f"{theirs['content'][:400]}"
        )
        assert theirs["content"] == posted["content"], (
            posted["content"],
            theirs["content"],
        )

    with step("editing the headline corrects it wherever the post reached"):
        plamenu_api.put(
            f"/api/v1/statuses/{posted['id']}",
            status=body,
            title=f"A better title {marker}",
        )
        wait_for(
            lambda: (
                plamenu2_api.get_status(theirs["id"])["title"]
                == f"A better title {marker}"
            ),
            desc="the Update(Article) carrying the new headline",
        )

    with step("the reverse direction: an article published by the peer"):
        their_title = f"Peer essay {marker}"
        plamenu2_api.post_status(
            f"Written on the other side. {marker}peer",
            post_kind="article",
            title=their_title,
        )
        ours = interop.delivered(plamenu_api, f"{marker}peer")
        assert ours["object_type"] == "Article", ours
        assert ours["title"] == their_title, ours
        assert ours["content"].count(their_title) == 1, ours["content"]


@pytest.mark.federation(direction="both")
def test_custom_emoji_round_trip(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, cli, marker
):
    """An `Emoji` tag riding a Note, and the image behind it.

    Covers: the shortcode arriving in the reader's `emojis` array, the image
    being proxied through the reader's own media route rather than hot-linked
    to the author's server, and the proxy actually serving the file."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )
    shortcode = unique("blobpeer")

    with step(f"the local instance defines :{shortcode}:"):
        path = emoji_file()
        try:
            cli.emoji_add(shortcode, path)
        finally:
            Path(path).unlink()
        codes = [e["shortcode"] for e in plamenu_api.get("/api/v1/custom_emojis")]
        assert shortcode in codes, codes

    with step("a post using it reaches the peer with the emoji attached"):
        plamenu_api.post_status(f"feeling :{shortcode}: today {marker}")
        theirs = interop.delivered(plamenu2_api, marker)
        emojis = {e["shortcode"]: e for e in theirs["emojis"]}
        assert shortcode in emojis, theirs["emojis"]
        url = emojis[shortcode]["url"]
        assert url.startswith(f"{plamenu2.url}/media/proxy/emoji/"), url
        assert plamenu_user.domain not in url, url

    with step("and the proxied image is served by the reader"):
        response = requests.get(url, verify=False, timeout=30)
        assert response.ok, response.status_code
        assert response.headers["content-type"].startswith("image/"), response.headers
        assert response.url.startswith(f"{plamenu2.url}/media/"), response.url


@pytest.mark.federation(direction="both")
def test_quote_handshake_and_revocation(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """FEP-044f end to end, with both roles played by this server.

    Covers: the outbound `QuoteRequest`, the quoted side's `Accept` carrying a
    `QuoteAuthorization`, the quoter verifying the stamp and settling at
    `accepted`, the quoted author's `quote` notification, and the author later
    revoking the authorization — which must take the quote back out of
    `accepted` on the quoter's server."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user posts something quotable"):
        posted = plamenu_api.post_status(f"quote me if you dare {marker}")
        theirs = interop.delivered(plamenu2_api, marker)

    with step("the peer quotes it; the handshake settles at accepted"):
        quote = plamenu2_api.post_status(
            f"look at this {marker}q", quoted_status_id=theirs["id"]
        )
        assert (quote.get("quote") or {}).get("state") == "pending", quote
        wait_for(
            lambda: plamenu2_api.quote_state(quote["id"]) == "accepted",
            timeout=120,
            desc="the Accept(QuoteRequest) and its stamp to be verified",
        )

    with step("the quoted author is notified and lists the quote"):
        wait_for(
            lambda: plamenu_api.notifications_from(plamenu2_user.acct, "quote"),
            desc="a quote notification for the quoted author",
        )
        listing = wait_for(
            lambda: plamenu_api.get(f"/api/v1/statuses/{posted['id']}/quotes") or None,
            desc="the quoting post to appear under the quotes listing",
        )
        assert listing[0]["uri"] == quote["uri"], listing

    with step("the author revokes the authorization they granted"):
        plamenu_api.post(
            f"/api/v1/statuses/{posted['id']}/quotes/{listing[0]['id']}/revoke"
        )
        wait_for(
            lambda: plamenu2_api.quote_state(quote["id"]) != "accepted",
            timeout=120,
            desc="the Delete(QuoteAuthorization) to unseat the quote",
        )
        log(f"quote state after revocation: {plamenu2_api.quote_state(quote['id'])}")


@pytest.mark.federation(direction="both")
def test_private_conversation_stays_one_conversation(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """A direct thread, as both participants' servers file it.

    Covers: direct addressing (`to` = the mentioned actor alone) read back as
    `direct` by the receiver, the unread flag and the read endpoint, and — the
    part only a peer that speaks FEP-171b can check — a two-message exchange
    ending up in exactly one conversation on *each* server rather than two."""
    interop.mutual_follow(
        plamenu_api, plamenu_user.acct, plamenu2_api, plamenu2_user.acct
    )

    with step("the local user sends a direct message"):
        sent = plamenu_api.post_status(
            f"@{plamenu2_user.acct} between us {marker}", visibility="direct"
        )
        assert sent["visibility"] == "direct", sent
        own = plamenu_api.conversation_containing(marker)
        assert own and own["unread"] is False, own

    with step("it arrives as an unread direct conversation"):
        theirs = wait_for(
            lambda: plamenu2_api.conversation_containing(marker),
            desc="the DM to appear in the recipient's conversations",
        )
        assert theirs["last_status"]["visibility"] == "direct", theirs
        assert theirs["unread"] is True, theirs
        assert [a["acct"] for a in theirs["accounts"]] == [plamenu_user.acct], theirs
        assert plamenu2_api.read_conversation(theirs["id"])["unread"] is False

    with step("the private reply threads into the same conversation, both sides"):
        plamenu2_api.post_status(
            f"@{plamenu_user.acct} agreed {marker}r",
            visibility="direct",
            in_reply_to_id=theirs["last_status"]["id"],
        )
        wait_for(
            lambda: (
                (conversation := plamenu_api.conversation_containing(f"{marker}r"))
                and conversation["id"] == own["id"]
                and conversation
            ),
            desc="the reply to land in the sender's existing conversation",
        )
        assert len([c for c in plamenu_api.conversations() if marker in str(c)]) == 1, (
            "a two-message exchange must be one conversation, not two"
        )
        assert (
            len([c for c in plamenu2_api.conversations() if marker in str(c)]) == 1
        ), "…and the same on the other side"


@pytest.mark.federation(direction="both")
def test_featured_collection_consent_handshake(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """FEP-7aa9 account collections: nobody is featured without agreeing.

    Covers: `Add(FeaturedCollection)` plus the `FeatureRequest` to the account
    being featured, that account's server auto-accepting for a discoverable
    profile, the returned `FeatureAuthorization` stamp landing in the served
    `FeaturedCollection` document, and the featured account seeing itself in
    its own `in_collections` listing."""
    with step("both accounts are discoverable, so either may feature the other"):
        plamenu_api.update_profile(discoverable="true")
        plamenu2_api.update_profile(discoverable="true")
        theirs = interop.resolve(plamenu_api, plamenu2_user.acct)

    with step("the local user creates a collection featuring the peer's account"):
        created = plamenu_api.post(
            "/api/v1/collections",
            name=f"good company {marker}",
            description="people worth reading",
            discoverable="true",
            **{"account_ids[]": theirs["id"]},
        )
        collection = created["collection"]
        assert collection["items"][0]["state"] == "pending", collection["items"]

    with step("the peer's server accepts the feature request"):
        item = wait_for(
            lambda: (
                shown["collection"]["items"][0]
                if (shown := plamenu_api.get(f"/api/v1/collections/{collection['id']}"))
                and shown["collection"]["items"][0]["state"] == "accepted"
                else None
            ),
            desc="the peer's Accept(FeatureRequest) to reach us",
        )
        log(f"membership accepted: {item['state']}")

    with step("the stamp shows in the served FeaturedCollection document"):
        doc = plamenu_api.ap_get(
            f"/users/{plamenu_user.username}/collections/{collection['id']}"
        )
        assert doc["type"] == "FeaturedCollection", doc
        stamps = [i.get("featureAuthorization", "") for i in doc["orderedItems"]]
        assert any(s.startswith(plamenu2.url) for s in stamps), doc["orderedItems"]

    with step("and the featured account can see where it is featured"):
        own = plamenu2_api.get("/api/v1/accounts/verify_credentials")["id"]
        listing = wait_for(
            lambda: (
                plamenu2_api.get(f"/api/v1/accounts/{own}/in_collections")[
                    "collections"
                ]
                or None
            ),
            desc="the collection to appear in the featured account's listing",
        )
        assert any(c["name"] == f"good company {marker}" for c in listing), listing


@pytest.mark.federation(direction="both")
def test_an_unseen_thread_is_backfilled_when_resolved(
    plamenu2, plamenu_user, plamenu_api, plamenu2_user, plamenu2_api, marker
):
    """Discovery by URL: a server that was never in the audience.

    Nobody on the peer follows the author here, so the thread arrives by
    dereference rather than delivery. Covers: search-by-URL resolving a status
    the instance has never seen, the ancestors being fetched with it so the
    reply is not an orphan, and the author's account being created from the
    actor document alone."""
    with step("the local user writes a thread nobody on the peer follows"):
        root = plamenu_api.post_status(f"unseen root {marker}")
        reply = plamenu_api.post_status(
            f"unseen reply {marker}", in_reply_to_id=root["id"]
        )

    with step("the peer resolves the reply by URL"):
        theirs = interop.ingested(plamenu2_api, reply["uri"])
        assert theirs["uri"] == reply["uri"], theirs
        assert theirs["account"]["acct"] == plamenu_user.acct, theirs["account"]

    with step("and pulls the root in behind it, so the thread is whole"):
        ancestors = wait_for(
            lambda: plamenu2_api.context(theirs["id"])["ancestors"] or None,
            desc="the peer to backfill the thread's ancestor",
        )
        assert [s["uri"] for s in ancestors] == [root["uri"]], ancestors
