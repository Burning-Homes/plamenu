"""Live Mastodon acceptance for both generations of local actor identity."""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import Api
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="inbound",
    peer="mastodon",
    one_way_reason=(
        "Mastodon discovering Plamenu's immutable local actor ID has no distinct "
        "reverse operation; Plamenu's URI-first remote discovery is covered by "
        "the ordinary Mastodon discovery suite."
    ),
)
def test_mastodon_resolves_an_immutable_numeric_actor(alice, plamenu_user):
    with step("Mastodon discovers a newly-created Plamenu account"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve the immutable-ID account"
        assert remote["uri"].startswith(
            f"https://{config.PLAMENU_DOMAIN}/ap/accounts/"
        ), remote["uri"]
        assert plamenu_user.username not in remote["uri"]


@pytest.mark.federation(direction="both", peer="mastodon")
def test_mastodon_round_trips_a_pre_upgrade_legacy_actor(alice, cli, db, marker):
    """The standing upgrade fixture predates numeric IDs. It must retain
    `/users/legacy` through upgrade while remaining fully federatable."""
    username = "legacy"
    acct = f"{username}@{config.PLAMENU_DOMAIN}"

    with step("Mastodon discovers the unchanged legacy actor URI"):
        remote = alice.resolve_account(acct)
        assert remote, "Mastodon could not resolve the legacy-ID account"
        assert remote["uri"] == f"https://{config.PLAMENU_DOMAIN}/users/{username}"

    with step("Mastodon follows the legacy actor and processes its Accept"):
        # Make the persistent fixture independent of a previous interrupted run.
        alice.unfollow(remote["id"])
        wait_for(
            lambda: db.follower_count(username) == 0,
            desc="any previous developer follow to be removed",
        )
        alice.follow(remote["id"])
        wait_for(
            lambda: alice.relationship(remote["id"])["following"],
            desc="Mastodon to process the legacy actor's Accept",
        )

    with step("a legacy-actor post reaches Mastodon"):
        post_marker = f"legacy actor post {marker}"
        cli.post(username, post_marker)
        received = wait_for(
            lambda: alice.home_status_containing(post_marker),
            desc="the legacy actor's post to enter Mastodon's home timeline",
        )

    with step("Mastodon replies back to the legacy actor"):
        reply_marker = f"legacy actor reply {marker}"
        alice.post_status(
            f"@{acct} {reply_marker}",
            in_reply_to_id=received["id"],
        )
        wait_for(
            lambda: db.status_id_containing(reply_marker),
            desc="Mastodon's reply to arrive in Plamenu",
        )

    with step("cleanup the persistent follow fixture"):
        alice.unfollow(remote["id"])
        wait_for(
            lambda: db.follower_count(username) == 0,
            desc="the legacy follow row to be removed",
        )


@pytest.mark.federation(
    direction="outbound",
    peer="mastodon",
    one_way_reason=(
        "This exercises Plamenu's local staged key rotation against a peer that "
        "cached the old keys; rotating Mastodon's signing keys is outside the "
        "peer harness and inbound multi-key refresh has integration coverage."
    ),
)
def test_mastodon_refetches_rotated_keys_during_overlap(
    alice, cli, db, marker, plamenu_user
):
    """A peer which cached the original actor must accept a delivery signed
    by the replacement RSA key. Both RSA and proof keys remain published for
    a bounded overlap so in-flight activities can still be verified."""
    plamenu = Api(config.PLAMENU_URL)

    with step("Mastodon follows and caches the actor's original keys"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve the pre-rotation actor"
        alice.follow(remote["id"])
        wait_for(
            lambda: alice.relationship(remote["id"])["following"],
            desc="Mastodon to process the pre-rotation Accept",
        )
        before = plamenu.ap_get(f"/users/{plamenu_user.username}")
        old_rsa = before["publicKey"]["id"]
        old_ed = next(
            method["id"]
            for method in before["assertionMethod"]
            if method["publicKeyMultibase"].startswith("z6Mk")
        )

    with step("Plamenu rotates transport and proof keys with overlap"):
        cli.rotate_account_key(plamenu_user.username, "rsa")
        cli.rotate_account_key(plamenu_user.username, "ed25519")
        after = plamenu.ap_get(f"/users/{plamenu_user.username}")
        method_ids = {method["id"] for method in after["assertionMethod"]}
        assert after["publicKey"]["id"] != old_rsa
        assert old_rsa in method_ids, "old RSA key disappeared before overlap ended"
        assert old_ed in method_ids, "old Ed25519 key disappeared before overlap ended"
        assert (
            len(
                [
                    method
                    for method in after["assertionMethod"]
                    if method["publicKeyMultibase"].startswith("z6Mk")
                ]
            )
            == 2
        )

    with step("Mastodon refetches the new key and accepts the delivery"):
        post_marker = f"rotated federation keys {marker}"
        cli.post(plamenu_user.username, post_marker)
        wait_for(
            lambda: alice.home_status_containing(post_marker),
            desc="a post signed by the rotated RSA key to reach Mastodon",
        )

    with step("cleanup the follow fixture"):
        alice.unfollow(remote["id"])
        wait_for(
            lambda: db.follower_count(plamenu_user.username) == 0,
            desc="the rotated-account follow row to be removed",
        )
