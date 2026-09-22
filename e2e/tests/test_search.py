"""/api/v2/search against the live Mastodon: remote discovery, search-by-URL
ingestion, and local full-text + hashtag search."""

import pytest
import requests
from plamenu_e2e import config, mastodon, plamenu, unique
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
    one_way_reason=(
        "recipient-authorized fetch: a local follower dereferences a protected "
        "remote object with their own actor key; this is a receive-side operation"
    ),
)
def test_recipient_signed_fetch_resolves_uncached_private_status(
    plamenu_user, plamenu_api, cli, db, marker
):
    """An entitled local follower can fetch an uncached followers-only Note.

    A different local actor is denied first, pinning both the signer boundary
    and the requirement that one principal's authorization failure must not
    suppress a later fetch by another principal.
    """
    with step("a fresh Mastodon author posts privately before having followers"):
        username = unique("authfetch")
        acct = f"{username}@{config.MASTODON_DOMAIN}"
        mastodon.create_account(username)
        author = mastodon.api_as(f"{username}@mastodon.local")
        remote = author.post_status(
            f"recipient fetch secret {marker}", visibility="private"
        )
        status_uri = remote["uri"]
        assert db.status_id_containing(marker) is None, (
            "the protected status must be uncached before either resolve"
        )

    with step("an unrelated local actor cannot resolve the protected object"):
        stranger = plamenu.User()
        cli.account_add(stranger)
        stranger_api = plamenu.login(stranger)
        denied = stranger_api.search(status_uri, resolve=True, type="statuses")[
            "statuses"
        ]
        assert denied == [], f"a non-follower resolved a private status: {denied!r}"
        failures = db.remote_fetch_failures_for_uri(status_uri)
        assert any(scope == "resource-account" for scope, _, _ in failures), (
            f"the denial was not recorded against the requesting actor: {failures!r}"
        )
        assert not any(scope == "resource" for scope, _, _ in failures), (
            "an authorization-shaped denial must not poison the global resource "
            f"budget: {failures!r}"
        )

    with step(f"@{plamenu_user.username} follows @{acct}"):
        account = plamenu_api.resolve_account(acct)
        assert account, "Plamenu could not resolve the fresh Mastodon author"
        plamenu_api.follow(account["id"])
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="Mastodon to accept the Plamenu user's follow",
        )

    with step("the entitled follower resolves the same still-uncached object"):
        assert db.status_id_containing(marker) is None, (
            "a pre-follow private post must not have arrived by fan-out"
        )
        statuses = plamenu_api.search(status_uri, resolve=True, type="statuses")[
            "statuses"
        ]
        assert statuses and marker in statuses[0]["content"], (
            "the follower's recipient-signed fetch did not return the private status "
            f"(got {statuses!r})"
        )
        assert statuses[0]["visibility"] == "private", statuses[0]

    with step("ingestion does not make the protected object visible to the stranger"):
        status_id = db.status_id_containing(marker)
        assert status_id is not None
        assert db.status_visibility(status_id) == "private"
        assert stranger_api.get_status_or_none(str(status_id)) is None


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


@pytest.mark.federation(
    direction="outbound",
    one_way_reason=(
        "same-direction refinement of test_mastodon_resolves_plamenu_account: "
        "the live WebFinger endpoint accepts the actor and human profile URLs "
        "that it publishes, in addition to the ordinary acct resource."
    ),
)
def test_webfinger_resolves_published_account_urls(plamenu_user):
    """Every published identity for a local account returns the same JRD."""

    def webfinger(resource: str):
        return requests.get(
            f"{config.PLAMENU_URL}/.well-known/webfinger",
            params={"resource": resource},
            timeout=30,
            verify=False,
        )

    with step("resolve the fresh account by its acct resource"):
        response = webfinger(f"acct:{plamenu_user.acct}")
        response.raise_for_status()
        expected = response.json()
        actor_url = next(
            link["href"]
            for link in expected["links"]
            if link["rel"] == "self" and link["type"] == "application/activity+json"
        )
        profile_url = expected["aliases"][0]

    with step("the canonical actor and human profile URLs return the same JRD"):
        for resource in (actor_url, profile_url):
            response = webfinger(resource)
            assert response.status_code == 200, (
                f"WebFinger rejected published resource {resource}: "
                f"{response.status_code} {response.text[:500]}"
            )
            assert response.json() == expected

    with step("unknown and foreign account URLs stay local 404s"):
        for resource in (
            f"{config.PLAMENU_URL}/@missing-{plamenu_user.username}",
            f"https://elsewhere.invalid/@{plamenu_user.username}",
        ):
            response = webfinger(resource)
            assert response.status_code == 404, (
                f"unexpected WebFinger result for {resource}: "
                f"{response.status_code} {response.text[:500]}"
            )
