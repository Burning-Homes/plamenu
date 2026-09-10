"""Visibility scopes federate faithfully in both directions.

`public` and `direct` have their own coverage (test_posts.py, test_direct.py);
this module covers `unlisted` and `private` (followers-only): the wire mapping
each peer reads back, timeline placement, and — the part that must *not*
happen — delivery or fetchability for non-followers and anonymous clients.
"""

import pytest
from plamenu_e2e import config, plamenu
from plamenu_e2e.api import Api, ApiError
from plamenu_e2e.steps import log, step, wait_for


def ap_get_status_code(uri: str) -> int:
    """Status code of an anonymous ActivityPub GET of a Plamenu object."""
    api = Api(config.PLAMENU_URL)
    r = api.http.get(uri, headers={"Accept": "application/activity+json"}, timeout=30)
    return r.status_code


@pytest.mark.federation(direction="both")
def test_unlisted_federates_both_ways(
    alice, plamenu_user, plamenu_api, cli, db, marker
):
    """Covers: `unlisted` on the wire in both directions — followers still
    receive it on home, both peers read the scope back as `unlisted`, and it
    stays off the public timelines on both sides."""
    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("plamenu posts unlisted; alice reads it back as unlisted"):
        local = plamenu_api.post_status(f"quiet post {marker}", visibility="unlisted")
        assert local["visibility"] == "unlisted", local
        status = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the unlisted Plamenu post on alice's home timeline",
        )
        assert status["visibility"] == "unlisted", status

    with step("the unlisted post stays off Plamenu's local public timeline"):
        listed = [s["id"] for s in plamenu_api.public_timeline(local=True)]
        assert local["id"] not in listed, (
            "an unlisted post must not appear on the public timeline"
        )

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts unlisted; plamenu stores the scope and homes it"):
        alice.post_status(f"quiet masto post {marker}b", visibility="unlisted")
        status_id = wait_for(
            lambda: db.status_id_containing(f"{marker}b"),
            desc="the unlisted Mastodon post to arrive at Plamenu",
        )
        assert db.status_visibility(status_id) == "unlisted"
        assert (
            wait_for(
                lambda: plamenu_api.home_status_containing(f"{marker}b"),
                desc="the unlisted post on the follower's home timeline",
            )["visibility"]
            == "unlisted"
        )

    with step("the inbound unlisted post stays off the federated timeline"):
        listed = [s["id"] for s in plamenu_api.public_timeline()]
        assert str(status_id) not in listed, (
            "an inbound unlisted post must not appear on the federated timeline"
        )


@pytest.mark.federation(direction="both")
def test_private_federates_both_ways(alice, plamenu_user, plamenu_api, cli, db, marker):
    """Covers: `private` (followers-only) in both directions — followers-only
    fan-out (a pre-follow private post is *never* delivered, proven via a
    post-follow fence), the scope read back as `private`, and the leak
    checks: anonymous AP fetch 404s, Mastodon cannot resolve it by URL, an
    anonymous or non-follower client-API view 404s."""
    with step("plamenu posts private while having no followers (the fence bait)"):
        undelivered = plamenu_api.post_status(
            f"secret before follow {marker}pre", visibility="private"
        )
        assert undelivered["visibility"] == "private", undelivered

    with step(f"alice follows @{plamenu_user.acct}"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted before posting",
        )

    with step("plamenu posts private again; only the follower copy arrives"):
        local = plamenu_api.post_status(
            f"secret after follow {marker}post", visibility="private"
        )
        status = wait_for(
            lambda: alice.home_status_containing(f"{marker}post"),
            desc="the followers-only post on alice's home timeline",
        )
        assert status["visibility"] == "private", status
        # The fence: the second private post arrived, so had the first one
        # been (wrongly) delivered it would have long landed too.
        assert alice.home_status_containing(f"{marker}pre") is None, (
            "a private post from before the follow must never be delivered"
        )

    with step("the private object leaks to no one"):
        # An anonymous AP GET never yields the object: under authorized fetch
        # (the default) it is rejected outright (401) before visibility is even
        # evaluated; with secure mode off it 404s on the visibility check. Both
        # mean "not served". The private-vs-non-follower distinction — that a
        # *signed* non-follower is refused too — is covered by the Mastodon
        # resolve-by-URL below (its instance-actor fetch is signed yet unlisted).
        code = ap_get_status_code(local["uri"])
        assert code in (401, 404), (
            f"anonymous AP GET of a private Note must not serve it, got {code}"
        )
        assert alice.resolve_status(undelivered["uri"]) is None, (
            "Mastodon must not be able to resolve a followers-only post by URL"
        )
        anonymous = Api(config.PLAMENU_URL)
        try:
            leaked = anonymous.get_status(local["id"])
        except ApiError as exc:
            assert "404" in str(exc), exc
        else:
            raise AssertionError(f"anonymous API view of a private post: {leaked}")

    with step("a non-follower on the same instance cannot view it either"):
        stranger = plamenu.User()
        cli = plamenu.Cli()
        cli.account_add(stranger)
        stranger_api = plamenu.login(stranger)
        assert stranger_api.get_status_or_none(local["id"]) is None, (
            "a non-follower must not see a followers-only post"
        )

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted (pending=false)",
        )

    with step("alice posts private; plamenu stores the scope, follower sees it"):
        alice.post_status(f"masto secret {marker}in", visibility="private")
        status_id = wait_for(
            lambda: db.status_id_containing(f"{marker}in"),
            desc="the followers-only Mastodon post to arrive at Plamenu",
        )
        assert db.status_visibility(status_id) == "private"
        homed = wait_for(
            lambda: plamenu_api.home_status_containing(f"{marker}in"),
            desc="the inbound private post on the follower's home timeline",
        )
        assert homed["visibility"] == "private", homed
        log(f"stored as status {status_id}")

    with step("the inbound private post is hidden from non-followers"):
        assert stranger_api.get_status_or_none(str(status_id)) is None, (
            "a non-follower must not see alice's followers-only post"
        )
        listed = [s["id"] for s in plamenu_api.public_timeline()]
        assert str(status_id) not in listed, (
            "an inbound private post must not appear on the federated timeline"
        )
