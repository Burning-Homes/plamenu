"""Long-form (`Article`) authoring against the live peer fleet.

The whole point of the kind is that the body federates. Whether that holds is
not a property of our code — it is a property of each receiver, and the answer
differs: Pleroma/Akkoma, Sharkey, GoToSocial and Mitra all render an `Article`
in full, while Mastodon converts it to a title-plus-link stub and discards the
body (`status_parser.rb#processed_text`). These tests pin *both* outcomes, so a
peer changing its mind shows up here rather than in someone's timeline.

They also pin the headline: `name` alone is invisible on four of these five
peers, which is why the title is baked into the body as a leading heading
(`LONGFORM_DESIGN.md` §2.1). Every assertion below that looks for the title in
the body text is checking that workaround still works.

All outbound: no peer in the fleet can *author* an `Article`, so there is no
reverse direction to pair with.
"""

import pytest
from plamenu_e2e import config, unique
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for

# The coverage audit reads markers statically, so each test spells its one-way
# reason out in full rather than sharing a constant.
BODY = (
    "This paragraph is deliberately longer than any title-plus-link stub could "
    "carry, and its final sentence is what proves the whole body arrived: "
)


def _publish_article(plamenu_api: Api, marker: str) -> tuple[dict, str]:
    """Publishes a long-form post; returns the status entity and its title."""
    title = f"Long-form {marker}"
    posted = plamenu_api.post_status(
        BODY + marker,
        post_kind="article",
        title=title,
    )
    assert posted["object_type"] == "Article", posted
    assert posted["title"] == title
    return posted, title


# ── Mastodon: the one peer that truncates ─────────────────────────────


@pytest.mark.federation(
    direction="outbound",
    peer="mastodon",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_reaches_mastodon_as_a_stub(alice, plamenu_user, plamenu_api, marker):
    """Plamenu -> Mastodon: the `Article` is converted to its title-plus-link
    stub. The body is *expected* to be missing — that is Mastodon's rule, and
    the composer warns the author about exactly this."""
    with step("alice follows the plamenu user"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, f"Mastodon could not resolve {plamenu_user.acct}"
        alice.follow(account["id"])

    with step("plamenu publishes long-form; Mastodon ingests the stub"):
        posted, title = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: alice.resolve_status(posted["uri"]),
            desc="the long-form post to be ingested by Mastodon",
        )

    with step("the stub carries the headline and a link back, not the body"):
        content = got["content"]
        assert title in content, content
        assert posted["url"] in content, content
        assert marker not in content.replace(title, ""), (
            "Mastodon renders converted types as title + link only; a body here "
            f"would mean the parser changed: {content}"
        )


# ── the peers that render it whole ────────────────────────────────────


@pytest.mark.federation(
    direction="outbound",
    peer="pleroma",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_reaches_akkoma_whole(pleroma_bob, plamenu_user, plamenu_api, marker):
    """Plamenu -> Akkoma: `ArticleNotePageValidator` accepts the type, so the
    full body arrives, headline included."""
    with step("bob follows the plamenu user"):
        account = pleroma_bob.resolve_account(plamenu_user.acct)
        assert account, f"Akkoma could not resolve {plamenu_user.acct}"
        pleroma_bob.follow(account["id"])

    with step("plamenu publishes long-form; Akkoma ingests it"):
        posted, title = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: pleroma_bob.resolve_status(posted["uri"]),
            desc="the long-form post to be ingested by Akkoma",
        )

    with step("body and headline both survive"):
        content = got["content"]
        assert marker in content, f"the body was truncated: {content}"
        assert title in content, f"the headline was dropped: {content}"
        assert not got.get("spoiler_text"), (
            "an Article with no CW must not arrive content-warned — the reason "
            f"we emit no excerpt in `summary`: {got.get('spoiler_text')!r}"
        )


@pytest.mark.federation(
    direction="outbound",
    peer="sharkey",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_reaches_sharkey_whole(
    sharkey_carol, plamenu_user, plamenu_api, marker
):
    """Plamenu -> Sharkey: `Article` is in `validPost`, so the note carries the
    whole body."""
    with step("carol follows the plamenu user"):
        user = sharkey_carol.resolve(
            f"https://{config.PLAMENU_DOMAIN}/users/{plamenu_user.username}"
        )
        sharkey_carol.follow(user["object"]["id"])

    with step("plamenu publishes long-form; Sharkey ingests it"):
        posted, title = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: sharkey_carol.resolve(posted["uri"]),
            desc="the long-form post to be ingested by Sharkey",
        )

    with step("body and headline both survive"):
        text = got["object"].get("text") or ""
        assert marker in text, f"the body was truncated: {text}"
        assert title in text, f"the headline was dropped: {text}"


@pytest.mark.federation(
    direction="outbound",
    peer="gts",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_reaches_gotosocial_whole(
    gts_dave, plamenu_user, plamenu_api, marker
):
    """Plamenu -> GoToSocial: `Article` resolves to a Statusable, so the body
    arrives whole (their converter has no title concept — the baked heading is
    what keeps the headline visible)."""
    with step("dave follows the plamenu user"):
        account = gts_dave.resolve_account(plamenu_user.acct)
        assert account, f"GoToSocial could not resolve {plamenu_user.acct}"
        gts_dave.follow(account["id"])

    with step("plamenu publishes long-form; GoToSocial ingests it"):
        posted, title = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: gts_dave.resolve_status(posted["uri"]),
            desc="the long-form post to be ingested by GoToSocial",
        )

    with step("body and headline both survive"):
        content = got["content"]
        assert marker in content, f"the body was truncated: {content}"
        assert title in content, f"the headline was dropped: {content}"


@pytest.mark.federation(
    direction="outbound",
    peer="mitra",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_reaches_mitra_whole(mitra_erin, plamenu_user, plamenu_api, marker):
    """Plamenu -> Mitra: a "converted" object keeps its content and gains a
    trailing link, so the body arrives whole."""
    with step("erin follows the plamenu user"):
        account = mitra_erin.resolve_account(plamenu_user.acct)
        assert account, f"Mitra could not resolve {plamenu_user.acct}"
        mitra_erin.follow(account["id"])

    with step("plamenu publishes long-form; Mitra ingests it"):
        posted, title = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: mitra_erin.resolve_status(posted["uri"]),
            desc="the long-form post to be ingested by Mitra",
        )

    with step("body and headline both survive"):
        content = got["content"]
        assert marker in content, f"the body was truncated: {content}"
        assert title in content, f"the headline was dropped: {content}"


# ── the edit path, where the two representations can drift ────────────


@pytest.mark.federation(
    direction="outbound",
    peer="pleroma",
    one_way_reason="no peer in the fleet can author an Article, so there is no inbound counterpart",
)
def test_long_form_headline_edit_reaches_akkoma(
    pleroma_bob, plamenu_user, plamenu_api, marker
):
    """Plamenu -> Akkoma: correcting the headline federates as an `Update`, and
    the baked heading is rebuilt from the new title rather than left stale."""
    with step("bob follows the plamenu user"):
        account = pleroma_bob.resolve_account(plamenu_user.acct)
        assert account, f"Akkoma could not resolve {plamenu_user.acct}"
        pleroma_bob.follow(account["id"])

    with step("plamenu publishes long-form; Akkoma ingests it"):
        posted, _ = _publish_article(plamenu_api, marker)
        got = wait_for(
            lambda: pleroma_bob.resolve_status(posted["uri"]),
            desc="the long-form post to be ingested by Akkoma",
        )

    with step("plamenu fixes the headline; the Update carries the new one"):
        fixed = f"Corrected {unique('headline')}"
        plamenu_api.put(f"/api/v1/statuses/{posted['id']}", title=fixed)
        wait_for(
            lambda: (
                fixed
                in (pleroma_bob.get_status_or_none(got["id"]) or {}).get("content", "")
            ),
            desc="the corrected headline to reach Akkoma",
        )
