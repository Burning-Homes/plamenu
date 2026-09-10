"""/api/v2/search against the live Mastodon: remote discovery, search-by-URL
ingestion, and local full-text + hashtag search."""

import pytest
from plamenu_e2e import config, mastodon, unique
from plamenu_e2e.steps import log, step, wait_for


@pytest.mark.federation(
    direction="inbound", reverse_of="test_mastodon_resolves_plamenu_account"
)
def test_resolve_discovers_fresh_mastodon_account(plamenu_api):
    """Covers: resolve=true account search (webfinger + actor fetch) for an
    account Plamenu has never seen, then plain db-backed account search for
    the now-known account."""
    username = unique("srch")
    acct = f"{username}@{config.MASTODON_DOMAIN}"

    with step(f"create a fresh Mastodon user @{acct} (never seen by Plamenu)"):
        mastodon.create_account(username)

    with step("resolve=true account search discovers it via webfinger"):
        accounts = plamenu_api.search(f"@{acct}", resolve=True, type="accounts")[
            "accounts"
        ]
        assert accounts and accounts[0]["acct"] == acct, (
            f"resolve search returned {accounts!r}"
        )
        log(f"discovered as {accounts[0]['acct']}")

    with step("without resolve, the now-known account is found in the database"):
        accounts = plamenu_api.search(acct, type="accounts")["accounts"]
        assert accounts and accounts[0]["acct"] == acct, (
            f"db-only search returned {accounts!r}"
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound URL-search ingest: resolving a remote status URL dereferences and stores it; a receive-side discovery behavior with no outbound counterpart.",
)
def test_search_by_url_ingests_remote_status(plamenu_api, alice, marker):
    """Covers: search-by-URL with resolve=true (fetch + ingestion of a remote
    status Plamenu has never seen) and full-text search over the ingested
    content."""
    with step("post on Mastodon as alice"):
        status_uri = alice.post_status(f"the {marker} move, freshly posted")["uri"]
        log(f"posted {status_uri}")

    with step("search-by-URL fetches and returns the status"):
        statuses = plamenu_api.search(status_uri, resolve=True)["statuses"]
        assert statuses and marker in statuses[0]["content"], (
            f"URL search did not return the status (got {statuses!r})"
        )

    with step("the ingested status is now full-text searchable"):
        hits = wait_for(
            lambda: plamenu_api.search(marker, type="statuses")["statuses"],
            desc="text search to find the ingested status",
        )
        assert hits[0]["account"]["acct"] == config.ALICE, (
            f"text search after ingestion returned {hits[0]['account']['acct']}"
        )


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="inbound permalink resolution: a permalink is followed to the canonical AP id and ingested; receive-side only, no outbound counterpart.",
)
def test_search_by_permalink_follows_to_canonical_id(plamenu_api, alice, marker):
    """Covers: search-by-URL with the browser permalink (a status's `url`,
    Mastodon's /@user/id form) rather than its canonical AP `id`. Resolution
    must follow the permalink to the canonical id and ingest the status."""
    with step("post on Mastodon as alice"):
        status = alice.post_status(f"the {marker} permalink case")
        permalink, canonical = status["url"], status["uri"]
        log(f"permalink {permalink} (canonical {canonical})")
        assert permalink != canonical, (
            f"expected permalink to differ from canonical id, both were {permalink}"
        )

    with step("search-by-permalink fetches, follows to the id, and returns it"):
        statuses = plamenu_api.search(permalink, resolve=True)["statuses"]
        assert statuses and marker in statuses[0]["content"], (
            f"permalink search did not return the status (got {statuses!r})"
        )
        assert statuses[0]["account"]["acct"] == config.ALICE, (
            f"permalink search returned wrong author {statuses[0]['account']['acct']}"
        )


def test_local_post_text_and_hashtag_search(plamenu_api, marker):
    """Covers: local status creation via the client API, full-text search of
    local posts, hashtag extraction and hashtag search."""
    tag = f"{marker}tag"

    with step(f"post locally with #{tag}"):
        plamenu_api.post_status(f"local {marker} thoughts #{tag}", visibility="public")

    with step("text search finds the local post"):
        statuses = plamenu_api.search(marker, type="statuses")["statuses"]
        assert any(marker in s["content"] for s in statuses), (
            f"text search returned {len(statuses)} statuses, none with the marker"
        )

    with step("hashtag search finds the tag"):
        hashtags = plamenu_api.search(tag, type="hashtags")["hashtags"]
        assert hashtags and hashtags[0]["name"] == tag, (
            f"hashtag search returned {hashtags!r}"
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_resolve_discovers_fresh_mastodon_account"
)
def test_mastodon_resolves_plamenu_account(alice, plamenu_user):
    """Covers: the reverse direction — Plamenu's webfinger and actor documents
    are good enough for Mastodon's resolve=true search to discover a fresh
    Plamenu account."""
    with step(f"alice resolves @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account and account["acct"] == plamenu_user.acct, (
            f"mastodon could not resolve our user (got {account!r})"
        )
