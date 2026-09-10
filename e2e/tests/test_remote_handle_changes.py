"""Live URI-first remote handle changes between two HTTPS Plamenu peers."""

import pytest
from plamenu_e2e import interop, unique
from plamenu_e2e.steps import step, wait_for


@pytest.mark.federation(direction="both", peer="plamenu2")
def test_live_handle_rename_reuse_and_cycles_preserve_actor_identity(
    plamenu2, plamenu2_user, plamenu2_api, plamenu_api, marker
):
    old = plamenu2_user.username
    renamed = unique("renamed")
    cycled = unique("cycled")

    with step("the local instance follows and caches the peer's original handle"):
        remote = interop.resolve(plamenu_api, plamenu2_user.acct)
        actor_uri = remote["uri"]
        remote_id = remote["id"]
        plamenu_api.follow(remote_id)
        wait_for(
            lambda: plamenu_api.relationship(remote_id)["following"],
            desc="the peer to accept the pre-rename follow",
        )

    with step("the peer changes only its handle; WebFinger loops to the same actor"):
        assert (
            plamenu2.db_value(
                "UPDATE accounts SET username = %s WHERE username = %s RETURNING username",
                renamed,
                old,
            )
            == renamed
        )
        refreshed = interop.resolve(plamenu_api, f"{renamed}@{plamenu2.domain}")
        assert refreshed["id"] == remote_id
        assert refreshed["uri"] == actor_uri
        assert plamenu_api.relationship(remote_id)["following"]

    with step("content and delivery continue through the preserved relationship"):
        post_marker = f"after live handle rename {marker}"
        plamenu2.commands.post(renamed, post_marker)
        wait_for(
            lambda: plamenu_api.home_status_containing(post_marker),
            desc="the renamed actor's post to arrive through the old follow",
        )

    with step("the abandoned handle can identify a different actor without merging"):
        replacement = plamenu2.account(prefix=old)
        # `account(prefix=...)` adds a uniqueness suffix; move it to the exact
        # vacated handle to exercise reuse rather than a merely similar name.
        assert (
            plamenu2.db_value(
                "UPDATE accounts SET username = %s WHERE username = %s RETURNING username",
                old,
                replacement.username,
            )
            == old
        )
        reused = interop.resolve(plamenu_api, f"{old}@{plamenu2.domain}")
        assert reused["id"] != remote_id
        assert reused["uri"] != actor_uri
        assert plamenu_api.relationship(remote_id)["following"]

    with step("the original actor can rename again and cycle back"):
        assert (
            plamenu2.db_value(
                "UPDATE accounts SET username = %s WHERE username = %s RETURNING username",
                cycled,
                renamed,
            )
            == cycled
        )
        second = interop.resolve(plamenu_api, f"{cycled}@{plamenu2.domain}")
        assert second["id"] == remote_id
        assert second["uri"] == actor_uri
        assert (
            plamenu2.db_value(
                "UPDATE accounts SET username = %s WHERE username = %s RETURNING username",
                renamed,
                cycled,
            )
            == renamed
        )
        returned = interop.resolve(plamenu_api, f"{renamed}@{plamenu2.domain}")
        assert returned["id"] == remote_id
        assert returned["uri"] == actor_uri

    with step("cleanup the follow fixture"):
        plamenu_api.unfollow(remote_id)
