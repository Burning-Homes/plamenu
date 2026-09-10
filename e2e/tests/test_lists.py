"""User lists with a federated member.

Lists are purely local, but their timelines carry federated statuses: a
Plamenu user lists alice@mastodon.local, her Mastodon post must land on the
list timeline, and flipping the list exclusive must pull it out of home.
"""

import pytest
from plamenu_e2e import config
from plamenu_e2e.api import ApiError
from plamenu_e2e.steps import log, step, wait_for


def test_list_timeline_carries_mastodon_member(
    plamenu_user, plamenu_api, alice, marker
):
    """Covers: POST/GET/PUT/DELETE /api/v1/lists, /lists/{id}/accounts,
    GET /api/v1/timelines/list/{id}, GET /api/v1/accounts/{id}/lists,
    exclusive-list home filtering — with a remote (Mastodon) list member."""
    with step(f"resolve and follow {config.ALICE}"):
        account = plamenu_api.resolve_account(config.ALICE)
        assert account, "Plamenu could not resolve alice"
        alice_id = account["id"]
        plamenu_api.follow(alice_id)
        wait_for(
            lambda: plamenu_api.relationship(alice_id)["following"],
            desc="the follow toward alice to be accepted",
        )

    with step("create a list and add alice to it"):
        created = plamenu_api.post("/api/v1/lists", title="e2e friends")
        list_id = created["id"]
        assert created["replies_policy"] == "list", f"unexpected default: {created!r}"
        assert created["exclusive"] is False
        plamenu_api.post(
            f"/api/v1/lists/{list_id}/accounts", **{"account_ids[]": alice_id}
        )
        members = plamenu_api.get(f"/api/v1/lists/{list_id}/accounts")
        assert [m["id"] for m in members] == [alice_id], (
            f"unexpected members: {members!r}"
        )
        containing = plamenu_api.get(f"/api/v1/accounts/{alice_id}/lists")
        assert [entry["id"] for entry in containing] == [list_id]

    with step("alice's Mastodon post lands on the list timeline (and home)"):
        alice.post_status(f"a post bound for a plamenu list {marker}")
        arrived = wait_for(
            lambda: next(
                (
                    s
                    for s in plamenu_api.get(f"/api/v1/timelines/list/{list_id}")
                    if marker in s["content"]
                ),
                None,
            ),
            desc="alice's post to appear on the list timeline",
        )
        log(f"status {arrived['id']} is on the list timeline")
        assert plamenu_api.home_status_containing(marker), (
            "a non-exclusive list must leave the member's posts on home"
        )

    with step("flipping the list exclusive pulls alice out of home"):
        updated = plamenu_api.put(f"/api/v1/lists/{list_id}", exclusive="true")
        assert updated["exclusive"] is True
        assert plamenu_api.home_status_containing(marker) is None, (
            "an exclusive list member's posts must leave the home timeline"
        )
        still_listed = [
            s
            for s in plamenu_api.get(f"/api/v1/timelines/list/{list_id}")
            if marker in s["content"]
        ]
        assert still_listed, "the post must stay on the list timeline"

    with step("deleting the list removes it"):
        assert plamenu_api.delete(f"/api/v1/lists/{list_id}") == {}
        with pytest.raises(ApiError, match="404"):
            plamenu_api.get(f"/api/v1/lists/{list_id}")
        assert plamenu_api.home_status_containing(marker), (
            "deleting the list must restore the member's posts to home"
        )
