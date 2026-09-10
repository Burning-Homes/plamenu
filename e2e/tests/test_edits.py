"""Status editing federates outbound: PUT /statuses/{id} re-distributes the
post as Update(Note), and Mastodon applies the edit. (The inbound direction
is covered by test_profile.py::test_status_edit_federates_from_mastodon.)"""

import pytest
from plamenu_e2e import unique
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(
    direction="outbound", reverse_of="test_status_edit_federates_from_mastodon"
)
def test_status_edit_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: PUT /api/v1/statuses/{id}, edit history, and the Update(Note)
    fan-out — Mastodon's copy changes content and gains an edit timestamp."""
    second_marker = unique("rewrite")

    with step(f"alice follows @{plamenu_user.acct} (so the post reaches her)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    with step("post through Plamenu, and the status reaches alice"):
        posted = plamenu_api.post_status(f"Furst wording {marker}")
        mastodon_copy = wait_for(
            lambda: alice.home_status_containing(marker),
            desc="the post to arrive on alice's home timeline",
        )
        assert mastodon_copy["edited_at"] is None

    with step("edit the post through Plamenu's client API"):
        edited = plamenu_api.edit_status(posted["id"], f"Fixed wording {second_marker}")
        assert second_marker in edited["content"]
        assert edited["edited_at"] is not None

        history = plamenu_api.get(f"/api/v1/statuses/{posted['id']}/history")
        assert [marker in v["content"] for v in history] == [True, False]
        source = plamenu_api.get(f"/api/v1/statuses/{posted['id']}/source")
        assert source["text"] == f"Fixed wording {second_marker}"

    with step("Mastodon applies the Update(Note)"):
        refreshed = wait_for(
            lambda: alice.home_status_containing(second_marker),
            desc="the edited content to reach Mastodon",
        )
        assert refreshed["id"] == mastodon_copy["id"], "same status, new text"
        assert refreshed["edited_at"] is not None, "Mastodon records the edit"
        assert alice.home_status_containing(marker) is None, "old wording gone"
