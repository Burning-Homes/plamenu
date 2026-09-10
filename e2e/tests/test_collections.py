"""Status collections over federation: Mastodon reads the likes/shares
collections we advertise (as untrusted engagement counts) and backfills
self-replies from the replies collection."""

import pytest
from plamenu_e2e import mastodon, unique
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="collection-serving: Mastodon reads the likes/shares/replies collections Plamenu advertises; Plamenu has no reciprocal ingest of a peer's engagement-count collections.",
)
def test_mastodon_ingests_advertised_collections(alice, plamenu_api, marker):
    """Covers: the Note's advertised `likes`/`shares` (count-only
    collections) and `replies` (inline first page of self-reply IRIs), plus
    the standalone collection endpoints Mastodon hits while resolving."""
    with step("a plamenu thread with engagement, never delivered to Mastodon"):
        parent = plamenu_api.post_status(f"collections root {marker}root")
        plamenu_api.post_status(
            f"collections self-reply {marker}reply", in_reply_to_id=parent["id"]
        )
        plamenu_api.post(f"/api/v1/statuses/{parent['id']}/favourite")
        plamenu_api.post(f"/api/v1/statuses/{parent['id']}/reblog")

    with step("alice discovers the root by URL search"):
        statuses = alice.search(parent["uri"], resolve=True)["statuses"]
        assert statuses and f"{marker}root" in statuses[0]["content"], (
            f"URL search did not return the status (got {statuses!r})"
        )
        fetched = statuses[0]

    with step("the advertised likes/shares became Mastodon's counts"):
        # Mastodon stores our collections' totalItems as untrusted counts
        # and serves them as favourites_count/reblogs_count.
        assert fetched["favourites_count"] == 1, fetched["favourites_count"]
        assert fetched["reblogs_count"] == 1, fetched["reblogs_count"]

    with step("the self-reply is backfilled from the replies collection"):

        def reply_in_context():
            context = alice.get(f"/api/v1/statuses/{fetched['id']}/context")
            return [
                s for s in context["descendants"] if f"{marker}reply" in s["content"]
            ] or None

        descendants = wait_for(
            reply_in_context,
            desc="Mastodon to fetch the advertised self-reply",
        )
        assert parent["account"]["acct"] in descendants[0]["account"]["acct"], (
            f"the backfilled reply has the wrong author: {descendants[0]['account']}"
        )


@pytest.mark.federation(
    direction="outbound",
    reverse_of="test_mastodon_status_tagged_collection_ingests_on_plamenu",
)
def test_status_tagged_collection_federates_to_mastodon(
    alice, plamenu_api, plamenu_user, marker
):
    """Covers: a local status linking a FEP-7aa9 collection folds it into the
    Note's `tag` as a `FeaturedCollection` (Mastodon's ProcessLinksService
    equivalent), Mastodon ingests it, and it surfaces in `tagged_collections`
    on both sides."""
    with step("plamenu creates a discoverable collection"):
        collection = plamenu_api.post(
            "/api/v1/collections",
            name=f"linked {marker}",
            description="a set worth linking",
            discoverable="true",
        )
        # create is wrapped in a `collection` root key (Mastodon `adapter: :json`)
        collection_id = collection["collection"]["id"]
        web_url = f"{plamenu_api.base_url}/@{plamenu_user.username}/collections/{collection_id}"

    with step("a status linking the collection tags it locally"):
        status = plamenu_api.post_status(f"see this set {marker} {web_url}")
        assert status["tagged_collections"], status
        assert status["tagged_collections"][0]["id"] == collection_id

    with step("mastodon resolves the status and ingests the tagged collection"):

        def resolved_with_collection():
            try:
                found = alice.search(status["uri"], resolve=True)["statuses"]
            except ApiError as e:
                # Mastodon 422s "Duplicate record" when a resolve races its own
                # concurrent AP fetch of the same URI against its unique index —
                # a known transient; retry rather than fail the wait_for.
                if "Duplicate record" in str(e):
                    return None
                raise
            if not found or f"{marker}" not in found[0]["content"]:
                return None
            return found[0] if found[0].get("tagged_collections") else None

        fetched = wait_for(
            resolved_with_collection,
            desc="Mastodon to ingest the tagged FeaturedCollection",
        )
        assert fetched["tagged_collections"][0]["name"] == f"linked {marker}", fetched[
            "tagged_collections"
        ]


@pytest.mark.federation(
    direction="inbound",
    one_way_reason="collection-serving: only the replies-collection backfill has a Plamenu counterpart; Plamenu deliberately never ingests a Note's likes/shares totals, so this is not a full reverse.",
)
def test_plamenu_ingests_mastodon_advertised_collections(
    alice, plamenu_user, plamenu_api, marker
):
    """Reverse of test_mastodon_ingests_advertised_collections, scoped to the
    replies backfill: opening the /context of a resolved-but-undelivered
    Mastodon root crawls the origin `replies` collection and ingests the
    self-reply, raising the locally-derived replies_count.

    (Plamenu derives favourites/reblogs counts from local rows and never
    ingests a remote Note's likes/shares totalItems — a deliberate divergence
    from Mastodon — so those counts are intentionally not asserted here.)"""
    with step("alice posts a root and a self-reply, never delivered to Plamenu"):
        root = alice.post_status(f"masto collroot {marker}root")
        alice.post_status(f"masto collreply {marker}reply", in_reply_to_id=root["id"])

    with step("a signed-in Plamenu user resolves only the root by URL"):
        local = wait_for(
            lambda: plamenu_api.resolve_status(root["uri"]),
            desc="Plamenu to resolve the root",
        )
        assert f"{marker}root" in local["content"]

    with step("opening the thread context enqueues the replies crawl"):
        # Opening /context is what triggers the origin `replies` collection
        # crawl; the self-reply (never delivered to Plamenu) is backfilled from
        # it. (No pre-crawl "reply absent" assertion — reading /context is
        # itself the trigger, so it would race the crawl it kicks off.)
        plamenu_api.context(local["id"])

    with step("Plamenu backfills the self-reply from the origin replies collection"):
        descendants = wait_for(
            lambda: (
                [
                    d
                    for d in plamenu_api.context(local["id"])["descendants"]
                    if f"{marker}reply" in d["content"]
                ]
                or None
            ),
            desc="the self-reply to be backfilled",
        )
        assert descendants[0]["account"]["acct"].startswith("alice")

    with step("the backfilled reply raises the locally-derived replies_count"):
        wait_for(
            lambda: plamenu_api.get_status(local["id"])["replies_count"] >= 1,
            desc="the root's replies_count to reflect the backfill",
        )


@pytest.mark.federation(
    direction="inbound",
    reverse_of="test_status_tagged_collection_federates_to_mastodon",
)
def test_mastodon_status_tagged_collection_ingests_on_plamenu(plamenu_api, marker):
    """Reverse of test_status_tagged_collection_federates_to_mastodon: a
    Mastodon status carrying a FeaturedCollection tag (injected via rails, since
    Mastodon never linkifies a .local URL) is ingested by Plamenu, surfacing in
    the remote status' `tagged_collections`.

    A FRESH Mastodon account is used (not the shared `alice`): the tag is served
    via `render_with_cache`, and alice's status AP payloads accumulate cached
    tag-less copies across the suite that race the injection; a fresh account has
    no such cache."""
    owner = unique("collowner")
    with step("a fresh Mastodon account creates a discoverable collection"):
        mastodon.create_account(owner)
        owner_api = mastodon.api_as(f"{owner}@mastodon.local")
        coll = owner_api.create_collection(
            f"linked {marker}",
            description="a set worth linking",
            discoverable="true",
            sensitive="false",
        )["collection"]
        collection_id = coll["id"]

    with step("it posts a status, then tags it with the collection via rails"):
        status = owner_api.post_status(f"see this set {marker}")
        mastodon.tag_status_with_collection(status["id"], collection_id)
        assert owner_api.get_status(status["id"])["tagged_collections"], (
            "tag not on the wire"
        )

    with step("Plamenu resolves the status and ingests the FeaturedCollection tag"):
        local = wait_for(
            lambda: plamenu_api.resolve_status(status["uri"]),
            desc="Plamenu to resolve the status",
        )
        fetched = wait_for(
            lambda: (
                plamenu_api.get_status(local["id"]).get("tagged_collections") or None
            ),
            desc="Plamenu to ingest the tagged collection",
        )
        assert fetched[0]["name"] == f"linked {marker}", fetched
        # The tagged-collection entity carries `account_id` (a string), not a
        # nested account; confirm ownership via the resolved status' author.
        assert local["account"]["acct"].startswith(owner), local["account"]
