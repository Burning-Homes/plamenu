"""Link preview cards: posted links are crawled and rendered as cards, and
locally-composed links federate as real anchors other servers can crawl."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for

# A real HTML page with OpenGraph metadata that the Plamenu server can fetch
# (remote from Plamenu's point of view, so it is eligible for crawling).
ABOUT_PAGE = f"{config.MASTODON_URL}/about"

# For the inbound direction the link must be one *Mastodon* linkifies, and
# twitter-text only accepts real TLDs — `.local` never becomes an anchor.
# example.com is reserved for exactly this; the crawl needs internet access.
EXTERNAL_PAGE = "https://example.com/"


@pytest.mark.federation(
    direction="outbound", reverse_of="test_link_preview_card_on_federated_mastodon_post"
)
def test_link_preview_card_on_plamenu_post(alice, plamenu_user, plamenu_api, marker):
    """Covers: URL linkification in composed statuses (Mastodon's anchor
    shape on the wire), the link-crawl queue, OpenGraph extraction, and the
    `card` attribute on the status entity."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post a link from Plamenu (the card is attached asynchronously)"):
        status = plamenu_api.post_status(f"reading {ABOUT_PAGE} {marker}")
        assert f'<a href="{ABOUT_PAGE}"' in status["content"], (
            "the URL must be linkified in the composed HTML"
        )

    with step("the crawl fetches the page and attaches an OpenGraph card"):
        card = wait_for(
            lambda: plamenu_api.get_status(status["id"])["card"],
            desc="the preview card to appear on the status",
        )
        assert card["url"] == ABOUT_PAGE
        assert card["type"] == "link"
        assert card["title"], "the card must carry the page's og:title"
        assert card["blurhash"] is None

    with step("alice receives the post with a crawlable anchor"):
        remote = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post to appear on alice's home timeline",
        )
        assert f'<a href="{ABOUT_PAGE}"' in remote["content"], (
            "the link must survive as a real anchor on Mastodon"
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_link_preview_card_on_plamenu_post"
)
def test_link_preview_card_on_federated_mastodon_post(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: anchor extraction from inbound (sanitized) remote HTML — a
    link in a Mastodon post gets a card on the Plamenu side too."""
    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts a link; the status reaches Plamenu with an anchor"):
        alice.post_status(f"worth a read {EXTERNAL_PAGE} {marker}")
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )
        status = plamenu_api.get_status(status_id)
        assert f'<a href="{EXTERNAL_PAGE}"' in status["content"], (
            "Mastodon must have linkified the URL"
        )

    with step("the crawl attaches a card to the remote status"):
        card = wait_for(
            lambda: plamenu_api.get_status(status_id)["card"],
            desc="the preview card to appear on the federated status",
        )
        assert card["url"] == EXTERNAL_PAGE
        assert card["title"], "the card must carry the page's title"
