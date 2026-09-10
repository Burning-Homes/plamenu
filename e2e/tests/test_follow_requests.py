"""Locked accounts and follow requests federate in both directions.

A locked Plamenu user advertises manuallyApprovesFollowers, holds inbound
Mastodon follows as pending requests, and authorize/reject federate Accept/
Reject back. Conversely, Plamenu following a locked Mastodon account stays
`requested` until alice approves it there.
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.steps import log, step, wait_for


def _lock(plamenu_api) -> None:
    entity = plamenu_api.patch("/api/v1/accounts/update_credentials", locked="true")
    assert entity["locked"] is True, f"expected locked account, got {entity!r}"


@pytest.mark.federation(
    direction="inbound", reverse_of="test_following_a_locked_mastodon_account"
)
def test_locked_follow_request_authorized(plamenu_user, plamenu_api, alice):
    """Covers: `locked` via update_credentials + actor document, inbound
    Follow held pending, GET /follow_requests, follow_request notification,
    POST /follow_requests/{id}/authorize -> federated Accept."""
    with step("lock the plamenu user and check the advertised actor flag"):
        _lock(plamenu_api)
        actor = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        assert actor["manuallyApprovesFollowers"] is True, (
            f"actor must advertise manual approval, got {actor.get('manuallyApprovesFollowers')!r}"
        )

    with step("alice resolves the user and sees the lock"):
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve our user"
        assert remote["locked"] is True, (
            f"Mastodon should see locked=true, got {remote!r}"
        )

    with step("alice's follow stays a pending request on both sides"):
        alice.follow(remote["id"])
        wait_for(
            lambda: alice.relationship(remote["id"])["requested"],
            desc="alice's relationship to show requested=true",
        )
        requests = wait_for(
            lambda: plamenu_api.get("/api/v1/follow_requests") or None,
            desc="the follow request to appear in plamenu's listing",
        )
        assert [r["acct"] for r in requests] == [config.ALICE], (
            f"follow_requests should list exactly alice, got {requests!r}"
        )
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        assert me["source"]["follow_requests_count"] == 1
        assert me["followers_count"] == 0, (
            "a pending request must not count as a follower"
        )
        kinds = [n["type"] for n in plamenu_api.get("/api/v1/notifications")]
        assert kinds == ["follow_request"], (
            f"expected a follow_request notification, got {kinds!r}"
        )
        requester_id = requests[0]["id"]
        log(f"alice is plamenu account id {requester_id}")

    with step("authorizing federates the Accept; alice ends up following"):
        rel = plamenu_api.post(f"/api/v1/follow_requests/{requester_id}/authorize")
        assert rel["followed_by"] is True, (
            f"authorize should yield followed_by=true, got {rel!r}"
        )
        wait_for(
            lambda: alice.relationship(remote["id"])["following"],
            desc="alice's relationship to become following=true (Accept processed)",
        )
        me = plamenu_api.get("/api/v1/accounts/verify_credentials")
        assert me["followers_count"] == 1, (
            "the authorized request should now count as a follower"
        )
        assert me["source"]["follow_requests_count"] == 0
        kinds = [n["type"] for n in plamenu_api.get("/api/v1/notifications")]
        assert kinds == ["follow"], (
            f"the notification should mature into follow, got {kinds!r}"
        )

    with step("cleanup: alice unfollows again"):
        alice.unfollow(remote["id"])


@pytest.mark.federation(
    direction="inbound", reverse_of="test_following_a_locked_mastodon_account_rejected"
)
def test_locked_follow_request_rejected(plamenu_user, plamenu_api, alice):
    """Covers: POST /follow_requests/{id}/reject -> federated Reject."""
    with step("lock the plamenu user; alice requests to follow"):
        _lock(plamenu_api)
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve our user"
        alice.follow(remote["id"])
        requests = wait_for(
            lambda: plamenu_api.get("/api/v1/follow_requests") or None,
            desc="the follow request to appear in plamenu's listing",
        )
        requester_id = requests[0]["id"]

    with step("rejecting federates the Reject; alice's request disappears"):
        plamenu_api.post(f"/api/v1/follow_requests/{requester_id}/reject")
        assert plamenu_api.get("/api/v1/follow_requests") == []
        wait_for(
            lambda: not alice.relationship(remote["id"])["requested"],
            desc="alice's relationship to drop requested after the Reject",
        )
        assert alice.relationship(remote["id"])["following"] is False
        kinds = [n["type"] for n in plamenu_api.get("/api/v1/notifications")]
        assert kinds == [], (
            f"the follow_request notification should be gone, got {kinds!r}"
        )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_locked_follow_request_authorized"
)
def test_following_a_locked_mastodon_account(plamenu_user, plamenu_api, alice, cli):
    """Covers: outbound Follow toward a locked account staying `requested`,
    Mastodon's follow_requests authorize -> our Accept(Follow) handling."""
    with step("alice locks her Mastodon account (restored afterwards)"):
        entity = alice.patch("/api/v1/accounts/update_credentials", locked="true")
        assert entity["locked"] is True

    try:
        with step(f"ensure {config.ALICE} is known to Plamenu"):
            account = plamenu_api.lookup(config.ALICE)
            if account is None:
                cli.post(
                    plamenu_user.username, f"wiring up @{config.ALICE} for an e2e run"
                )
                account = wait_for(
                    lambda: plamenu_api.lookup(config.ALICE),
                    desc="the mention to seed alice into plamenu's accounts",
                )
            alice_id = account["id"]

        with step("the follow stays pending until alice approves it"):
            plamenu_api.follow(alice_id)
            request = wait_for(
                lambda: next(
                    (
                        r
                        for r in alice.get("/api/v1/follow_requests")
                        if r["acct"] == plamenu_user.acct
                    ),
                    None,
                ),
                desc="the request to reach alice's follow_requests",
            )
            rel = plamenu_api.relationship(alice_id)
            assert rel["requested"] is True, f"expected requested=true, got {rel!r}"
            assert rel["following"] is False

        with step("alice authorizes; the Accept lands and we follow"):
            alice.post(f"/api/v1/follow_requests/{request['id']}/authorize")
            wait_for(
                lambda: plamenu_api.relationship(alice_id)["following"],
                desc="plamenu relationship to become following=true (Accept processed)",
            )

        with step("cleanup: unfollow alice again"):
            plamenu_api.unfollow(alice_id)
    finally:
        entity = alice.patch("/api/v1/accounts/update_credentials", locked="false")
        assert entity["locked"] is False, "alice must end the test unlocked"


@pytest.mark.federation(
    direction="outbound", reverse_of="test_locked_follow_request_rejected"
)
def test_following_a_locked_mastodon_account_rejected(
    plamenu_user, plamenu_api, alice, cli
):
    """Reverse companion of the authorize case: alice REJECTS Plamenu's pending
    follow, and the inbound Reject(Follow) must clear Plamenu's `requested`
    flag (no stuck pending state)."""
    with step("alice locks her Mastodon account (restored afterwards)"):
        entity = alice.patch("/api/v1/accounts/update_credentials", locked="true")
        assert entity["locked"] is True

    try:
        with step(f"ensure {config.ALICE} is known to Plamenu"):
            account = plamenu_api.lookup(config.ALICE)
            if account is None:
                cli.post(
                    plamenu_user.username, f"wiring up @{config.ALICE} for an e2e run"
                )
                account = wait_for(
                    lambda: plamenu_api.lookup(config.ALICE),
                    desc="the mention to seed alice into plamenu's accounts",
                )
            alice_id = account["id"]

        with step("plamenu follows; the request stays pending on both sides"):
            plamenu_api.follow(alice_id)
            request = wait_for(
                lambda: next(
                    (
                        r
                        for r in alice.get("/api/v1/follow_requests")
                        if r["acct"] == plamenu_user.acct
                    ),
                    None,
                ),
                desc="the request to reach alice's follow_requests",
            )
            rel = plamenu_api.relationship(alice_id)
            assert rel["requested"] is True and rel["following"] is False, rel

        with step("alice rejects; the federated Reject clears requested"):
            alice.post(f"/api/v1/follow_requests/{request['id']}/reject")
            wait_for(
                lambda: not plamenu_api.relationship(alice_id)["requested"],
                desc="Plamenu's requested flag to clear after the Reject",
            )
            assert plamenu_api.relationship(alice_id)["following"] is False
    finally:
        entity = alice.patch("/api/v1/accounts/update_credentials", locked="false")
        assert entity["locked"] is False, "alice must end the test unlocked"
