"""Featured hashtags federate in both directions: featuring a tag is an Add of
a Hashtag object targeting the account's featured collection, unfeaturing a
Remove. Mastodon shows (then drops) the featured tag on the remote profile, and
Plamenu records a Mastodon-side featured tag the same way."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_featured_tag_federates_from_mastodon"
)
def test_featured_tag_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: POST /featured_tags and DELETE /featured_tags/{id}, plus the
    Add/Remove(Hashtag) fan-out — Mastodon shows then drops the featured tag on
    the remote profile."""
    with step(f"alice follows @{plamenu_user.acct} (so the Add reaches her)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    with step("feature a tag through Plamenu; it lists it"):
        featured = plamenu_api.post("/api/v1/featured_tags", name=f"#{marker}")
        assert featured["name"] == marker
        listing = plamenu_api.get("/api/v1/featured_tags")
        assert [t["name"] for t in listing] == [marker]

    with step("Mastodon records the featured tag on the remote profile (Add)"):
        wait_for(
            lambda: any(
                t["name"] == marker
                for t in alice.get(f"/api/v1/accounts/{account['id']}/featured_tags")
            ),
            desc="the featured tag to appear on the profile as seen from Mastodon",
        )

    with step("unfeature; Mastodon drops it again (Remove)"):
        plamenu_api.delete(f"/api/v1/featured_tags/{featured['id']}")
        assert plamenu_api.get("/api/v1/featured_tags") == []
        wait_for(
            lambda: (
                not any(
                    t["name"] == marker
                    for t in alice.get(
                        f"/api/v1/accounts/{account['id']}/featured_tags"
                    )
                )
            ),
            desc="the featured tag to disappear from the Mastodon-side profile",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_featured_tag_federates_to_mastodon"
)
def test_featured_tag_federates_from_mastodon(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: inbound Add/Remove of a Hashtag targeting the sender's featured
    collection — alice's featured tag shows up in Plamenu's account featured-tag
    listing and goes away again."""
    with step(f"@{plamenu_user.username} follows {config.ALICE} (so the Add arrives)"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    remote_alice = plamenu_api.lookup(config.ALICE)
    assert remote_alice, "Plamenu must already know alice"
    alice_account = alice.get("/api/v1/accounts/verify_credentials")

    with step("alice features a tag; Plamenu records it (Add)"):
        alice.post("/api/v1/featured_tags", name=f"#{marker}")
        wait_for(
            lambda: any(
                t["name"] == marker
                for t in plamenu_api.get(
                    f"/api/v1/accounts/{remote_alice['id']}/featured_tags"
                )
            ),
            desc="the featured tag to appear in Plamenu's listing",
        )

    with step("alice unfeatures it; Plamenu drops it again (Remove)"):
        featured = next(
            t
            for t in alice.get(f"/api/v1/accounts/{alice_account['id']}/featured_tags")
            if t["name"] == marker
        )
        alice.delete(f"/api/v1/featured_tags/{featured['id']}")
        wait_for(
            lambda: (
                not any(
                    t["name"] == marker
                    for t in plamenu_api.get(
                        f"/api/v1/accounts/{remote_alice['id']}/featured_tags"
                    )
                )
            ),
            desc="the featured tag to disappear from Plamenu's listing",
        )


def test_featured_tags_backfill_on_discovery(plamenu_api, marker):
    """Covers the `featuredTags` actor property: a never-before-seen Mastodon
    account with an existing featured tag is discovered by Plamenu, which
    dereferences the advertised collection and backfills the profile's
    featured tags (Mastodon's featured-tags collection sync job)."""
    from plamenu_e2e import mastodon

    owner = f"tagowner{marker[-8:]}"
    tag = f"backfill{marker[-6:]}"
    with step(f"a fresh Mastodon account @{owner} features #{tag}"):
        mastodon.create_account(owner)
        owner_api = mastodon.api_as(f"{owner}@mastodon.local")
        owner_api.post("/api/v1/featured_tags", name=f"#{tag}")

    with step("Plamenu discovers the account and backfills the tag"):
        account = plamenu_api.resolve_account(f"{owner}@mastodon.local")
        assert account, "Plamenu could not resolve the fresh account"
        wait_for(
            lambda: (
                [
                    t["name"]
                    for t in plamenu_api.get(
                        f"/api/v1/accounts/{account['id']}/featured_tags"
                    )
                ]
                == [tag]
            ),
            desc="the featuredTags collection sync to land",
        )


@pytest.mark.federation(
    direction="outbound",
    one_way_reason="one-way backfill: Mastodon fetches Plamenu's featuredTags collection on first discovery (no Add is ever sent), so there is no inbound counterpart to pair.",
)
def test_mastodon_backfills_plamenu_featured_tags_on_discovery(
    alice, plamenu_user, plamenu_api, marker
):
    """Reverse of test_featured_tags_backfill_on_discovery: Plamenu advertises +
    serves a `featuredTags` collection, so when Mastodon discovers the account
    for the first time it fetches the collection and backfills the tag — with no
    Add(Hashtag) ever sent (the account has no followers)."""
    with step("a fresh Plamenu user features a hashtag before Mastodon sees it"):
        plamenu_api.post("/api/v1/featured_tags", name=f"#{marker}")
        assert [t["name"] for t in plamenu_api.get("/api/v1/featured_tags")] == [marker]

    with step("alice discovers the account for the first time"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the fresh account"

    with step("Mastodon backfills the featuredTags collection on discovery"):
        wait_for(
            lambda: any(
                t["name"] == marker
                for t in alice.get(f"/api/v1/accounts/{account['id']}/featured_tags")
            ),
            desc="the featuredTags sync to land on the Mastodon-side profile",
        )
