"""Custom emoji federate in both directions: inbound `Emoji` tags are
recorded and rendered, and local emoji ride outbound Notes for Mastodon
to ingest."""

import base64
import tempfile
from pathlib import Path

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import step, wait_for

# A 1x1 transparent PNG — small enough for every emoji size limit.
PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhf"
    "DwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


@pytest.mark.federation(
    direction="inbound", reverse_of="test_emoji_federates_to_mastodon"
)
def test_emoji_federates_from_mastodon(alice, plamenu_user, cli, db, marker):
    """Covers: inbound `Emoji` tag on Create(Note) → `custom_emojis` row
    under the sender's domain, and the status entity re-scanning its text
    into a populated `emojis` array."""
    shortcode = unique("blobmast")

    with step(f"create custom emoji :{shortcode}: on Mastodon"):
        mastodon.create_emoji(shortcode, PNG_BASE64)

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts with the emoji; the Emoji tag must be recorded"):
        alice.post_status(f"hello :{shortcode}: {marker}")
        wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in plamenu's statuses table",
        )
        image_url = db.emoji_image_url(shortcode, config.MASTODON_DOMAIN)
        assert image_url and image_url.startswith("https://"), (
            f"emoji row missing or imageless: {image_url!r}"
        )

    with step("the status entity carries the emoji"):
        status_id = db.status_id_containing(marker)
        from plamenu_e2e.api import Api

        api = Api(config.PLAMENU_URL)
        entity = api.get(f"/api/v1/statuses/{status_id}")
        emojis = {e["shortcode"]: e for e in entity["emojis"]}
        assert shortcode in emojis, (
            f"emojis array lacks :{shortcode}:: {entity['emojis']}"
        )
        # The remote emoji image is proxied through Plamenu, never hot-linked
        # to Mastodon — the privacy guarantee.
        url = emojis[shortcode]["url"]
        assert url.startswith(f"{config.PLAMENU_URL}/media/proxy/emoji/"), url
        assert config.MASTODON_DOMAIN not in url

    with step("the proxied emoji image is fetched and served by Plamenu"):
        resp = api.http.get(url, timeout=30)
        assert resp.ok and resp.headers["content-type"].startswith("image/"), (
            f"proxy did not serve the emoji image: {resp.status_code}"
        )
        assert resp.url.startswith(f"{config.PLAMENU_URL}/media/"), (
            f"proxy must resolve to a Plamenu file, got {resp.url}"
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_emoji_federates_from_mastodon"
)
def test_emoji_federates_to_mastodon(alice, plamenu_user, plamenu_api, cli, marker):
    """Covers: local emoji creation via the CLI, the picker listing, the
    outbound `Emoji` tag on Create(Note), and Mastodon ingesting it into the
    status' `emojis` array."""
    shortcode = unique("blobpla")

    with step(f"create local emoji :{shortcode}: via the CLI"):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(base64.b64decode(PNG_BASE64))
            png_path = f.name
        cli.emoji_add(shortcode, png_path)
        Path(png_path).unlink()

    with step("the emoji appears in the picker listing"):
        listing = plamenu_api.get("/api/v1/custom_emojis")
        codes = [e["shortcode"] for e in listing]
        assert shortcode in codes, f"picker listing lacks :{shortcode}:: {codes}"

    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("post with the emoji; Mastodon must ingest the Emoji tag"):
        plamenu_api.post_status(f"time to :{shortcode}: {marker}")
        status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the Plamenu post to appear on alice's home timeline",
        )
        emojis = {e["shortcode"]: e for e in status["emojis"]}
        assert shortcode in emojis, (
            f"Mastodon's status entity lacks :{shortcode}:: {status['emojis']}"
        )
        assert emojis[shortcode]["url"], "Mastodon stored no image for the emoji"
