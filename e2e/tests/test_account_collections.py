"""Account collections (FEP-7aa9) over real federation.

The outbound consent handshake against Mastodon 4.6: a Plamenu collection
features a remote Mastodon account; Mastodon (auto-)accepts the
`FeatureRequest` and returns its `FeatureAuthorization` stamp, which we record
and surface in the served `FeaturedCollection` document.

The inverse path verifies Plamenu's actor-level `interactionPolicy.canFeature`
interop: Mastodon refuses remote accounts that do not advertise it, creates a
pending membership when it is present, and accepts our returned
`FeatureAuthorization`.
"""

import pytest
from plamenu_e2e import config, mastodon
from plamenu_e2e.shapes import assert_same_shape
from plamenu_e2e.steps import log, step, wait_for


@pytest.fixture(scope="module", autouse=True)
def _clean_mastodon_collections(alice):
    """Start with headroom without exercising a known Mastodon alpha bug.

    Collections created by this module are intentionally left for the next
    run to clear locally. Deleting a remote-member collection through the
    pinned Mastodon nightly queues a delivery to its local owner's blank inbox
    and can race an Add worker after deleting the collection row.
    """
    mastodon.clear_collections("alice")


@pytest.mark.federation(direction="both")
def test_collection_response_shapes_match_mastodon(
    alice, plamenu_api, plamenu_user, marker
):
    """The account-collections REST surface must return byte-for-byte the same
    response *shapes* as Mastodon — 3rd-party clients depend on the exact root
    key (`adapter: :json`), the enveloped-vs-bare list form, and every field
    name/type. This runs the identical sequence against both servers and diffs
    the skeletons, so any drift (the bare-array/bare-object regression that
    broke real clients) fails here instead of shipping green."""
    with step("both accounts discoverable, so each can feature the other"):
        alice.update_profile(discoverable="true")
        plamenu_api.update_profile(discoverable="true")
        plamenu_member = plamenu_api.resolve_account(config.ALICE)["id"]
        masto_member = alice.resolve_account(plamenu_user.acct)["id"]

    def sequence(api, member_id):
        """create -> add a (remote) member -> read it back four ways."""
        out = {}
        out["create"] = api.post(
            "/api/v1/collections",
            name=f"shape {marker}",
            description="d",
            sensitive="false",  # Mastodon 422s without it; harmless on Plamenu
            discoverable="true",
        )
        cid = out["create"]["collection"]["id"]
        me = api.get("/api/v1/accounts/verify_credentials")["id"]
        out["add_item"] = api.post(
            f"/api/v1/collections/{cid}/items", account_id=member_id
        )
        # Snapshot the reads *after* adding, so the inline `items[]` element
        # shape (not just the empty-list wrapper) is exercised too.
        out["index"] = api.get(f"/api/v1/accounts/{me}/collections")
        out["show"] = api.get(f"/api/v1/collections/{cid}")
        out["in_collections"] = api.get(f"/api/v1/accounts/{me}/in_collections")
        return out

    with step("run the sequence on Mastodon (the reference shapes)"):
        m = sequence(alice, masto_member)
    with step("run the identical sequence on Plamenu"):
        p = sequence(plamenu_api, plamenu_member)

    try:
        for endpoint in ("create", "add_item", "index", "show", "in_collections"):
            with step(f"{endpoint} response shape matches Mastodon"):
                # `accounts` in `show` embeds full Account entities; account-
                # serializer parity is its own concern, so diff only the
                # collection envelope here.
                ignore = ("accounts",) if endpoint == "show" else ()
                assert_same_shape(endpoint, m[endpoint], p[endpoint], ignore=ignore)
                log(f"{endpoint}: shapes identical")
    finally:
        # Plamenu's collection is ours to clean through the API. The module
        # fixture clears Mastodon's disposable reference rows at the start of
        # the next run without triggering its alpha deletion bugs.
        plamenu_api.delete(f"/api/v1/collections/{p['create']['collection']['id']}")


@pytest.mark.federation(
    direction="outbound", reverse_of="test_mastodon_collection_features_plamenu_account"
)
def test_plamenu_collection_features_mastodon_account(
    alice, plamenu_api, plamenu_user, marker
):
    """Covers: outbound `Add(FeaturedCollection)` + `FeatureRequest`, Mastodon
    auto-accept of a discoverable local account, inbound
    `Accept(FeatureRequest)` carrying the stamp, and the granted authorization
    surfacing in our served `FeaturedCollection` document."""
    with step("alice is discoverable, so a remote may feature her"):
        alice.patch("/api/v1/accounts/update_credentials", discoverable="true")

    with step("plamenu resolves alice@mastodon.local"):
        account = plamenu_api.resolve_account(config.ALICE)
        assert account, f"could not resolve {config.ALICE}"
        log(f"alice is account {account['id']} on plamenu")

    with step("plamenu creates a collection featuring alice"):
        created = plamenu_api.post(
            "/api/v1/collections",
            name=f"mutuals {marker}",
            description="people I like",
            discoverable="true",
            **{"account_ids[]": account["id"]},
        )
        # Mastodon wraps create in a `collection` root key (`adapter: :json`);
        # a 3rd-party client reads `data.collection`. Assert the envelope, not
        # just the payload — the bare-object shape is what broke real clients.
        assert set(created) == {"collection"}, f"create envelope drifted: {created}"
        collection = created["collection"]
        collection_id = collection["id"]
        assert collection["items"], collection
        # A remote member is pending until it answers our FeatureRequest.
        assert collection["items"][0]["state"] == "pending", collection["items"]

    with step("mastodon accepts the feature request"):

        def accepted():
            shown = plamenu_api.get(f"/api/v1/collections/{collection_id}")
            item = shown["collection"]["items"][0]
            return item if item["state"] == "accepted" else None

        item = wait_for(accepted, desc="Mastodon to accept the feature request")
        log(f"membership accepted: {item}")

    with step("the granted stamp surfaces in our FeaturedCollection document"):
        doc = plamenu_api.ap_get(
            f"/users/{plamenu_user.username}/collections/{collection_id}"
        )
        assert doc["type"] == "FeaturedCollection", doc
        featured = doc.get("orderedItems", [])
        assert featured, f"no FeaturedItems served: {doc}"
        stamps = [i.get("featureAuthorization", "") for i in featured]
        assert any(s.startswith(config.MASTODON_URL) for s in stamps), (
            f"alice's FeaturedItem carries no Mastodon stamp: {featured}"
        )

    with step("removing the item retracts the membership"):
        plamenu_api.delete(f"/api/v1/collections/{collection_id}/items/{item['id']}")
        shown = plamenu_api.get(f"/api/v1/collections/{collection_id}")
        assert shown["collection"]["item_count"] == 0, shown["collection"]


@pytest.mark.federation(
    direction="inbound", reverse_of="test_plamenu_collection_features_mastodon_account"
)
def test_mastodon_collection_features_plamenu_account(
    alice, plamenu_api, plamenu_user, marker
):
    """Covers: Plamenu advertises actor `interactionPolicy.canFeature`,
    Mastodon permits a collection item for that remote account, Plamenu accepts
    the inbound `FeatureRequest`, and Mastodon records the returned stamp."""
    with step("plamenu_user is discoverable, so Mastodon may feature them"):
        own = plamenu_api.update_profile(discoverable="true")
        account_id = own["id"]
        actor = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        assert actor["discoverable"] is True, actor
        approvals = actor["interactionPolicy"]["canFeature"]["automaticApproval"]
        assert "https://www.w3.org/ns/activitystreams#Public" in approvals, actor

    with step("alice resolves the Plamenu actor as featureable"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, f"could not resolve {plamenu_user.acct}"
        policy = remote.get("feature_approval") or {}
        assert policy.get("current_user") == "automatic", remote
        log(f"plamenu_user is Mastodon account {remote['id']}")

    with step("alice creates a collection featuring the Plamenu account"):
        collection = alice.post(
            "/api/v1/collections",
            name=f"remote pals {marker}",
            description="people I like",
            sensitive="false",
            discoverable="true",
            **{"account_ids[]": remote["id"]},
        )
        collection_entity = collection.get("collection") or collection
        collection_id = collection_entity["id"]
        assert collection_entity["items"], collection
        assert collection_entity["items"][0]["state"] == "pending", collection_entity[
            "items"
        ]

    with step("Plamenu accepts the feature request and stores the remote collection"):

        def seen_by_plamenu():
            body = plamenu_api.get(f"/api/v1/accounts/{account_id}/in_collections")
            # `in_collections` is `{collections: [...]}` (Mastodon `adapter: :json`).
            assert set(body) == {"collections"}, (
                f"in_collections envelope drifted: {body}"
            )
            return next(
                (row for row in body["collections"] if marker in row["name"]), None
            )

        stored = wait_for(
            seen_by_plamenu,
            desc="Plamenu to ingest Mastodon's collection containing the local account",
        )
        assert stored["local"] is False, stored

    with step("Mastodon records Plamenu's FeatureAuthorization"):

        def accepted_on_mastodon():
            shown = alice.get(f"/api/v1/collections/{collection_id}")
            entity = shown.get("collection") or shown
            item = entity["items"][0]
            return item if item["state"] == "accepted" else None

        item = wait_for(
            accepted_on_mastodon,
            desc="Mastodon to mark the Plamenu membership accepted",
        )
        log(f"mastodon membership accepted: {item}")
