"""Profile identity (M6) federates in both directions: update_credentials
with Update(Actor) fan-out, and inbound Update(Actor)/Update(Note)."""

import struct
import zlib

import pytest
from plamenu_e2e import config, unique
from plamenu_e2e.steps import step, wait_for


def tiny_png(rgb=(200, 30, 30)) -> bytes:
    """A 1x1 PNG, built by hand so the suite needs no image library."""

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + kind
            + payload
            + struct.pack(">I", zlib.crc32(kind + payload))
        )

    ihdr = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
    idat = zlib.compress(b"\x00" + bytes(rgb))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", idat)
        + chunk(b"IEND", b"")
    )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_profile_edit_federates_from_mastodon"
)
def test_profile_edit_federates_to_mastodon(alice, plamenu_user, plamenu_api, marker):
    """Covers: update_credentials (display name, bio, avatar upload, actor
    flags) and the Update(Actor) fan-out — Mastodon's copy changes."""
    new_name = f"Renamed {marker}"

    with step(f"alice follows @{plamenu_user.acct} (so Updates reach her server)"):
        account = alice.resolve_account(plamenu_user.acct)
        assert account, "Mastodon could not resolve the account"
        alice.follow(account["id"])
        wait_for(
            lambda: alice.relationship(account["id"])["following"],
            desc="the follow to be accepted",
        )

    with step("edit the profile through Plamenu's client API"):
        entity = plamenu_api.update_profile(
            files={"avatar": ("avatar.png", tiny_png(), "image/png")},
            display_name=new_name,
            note=f"bio {marker}",
            bot="true",
            indexable="true",
            hide_collections="true",
        )
        assert entity["display_name"] == new_name
        assert f"bio {marker}" in entity["note"]
        assert "/media/" in entity["avatar"]
        assert entity["bot"] is True
        assert entity["indexable"] is True
        assert entity["hide_collections"] is True

    with step("the Plamenu actor document exposes the federated actor flags"):
        actor = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        assert actor["type"] == "Service", actor
        assert actor["indexable"] is True, actor
        assert "hideCollections" not in actor, (
            "hide_collections is local-only and must not be federated"
        )

    with step("Mastodon applies the Update(Actor)"):
        wait_for(
            lambda: alice.lookup(plamenu_user.acct)["display_name"] == new_name,
            desc="the new display name to reach Mastodon",
        )
        refreshed = alice.lookup(plamenu_user.acct)
        assert marker in refreshed["note"]
        assert refreshed["bot"] is True
        assert "missing" not in refreshed["avatar"], (
            "Mastodon should have fetched the avatar"
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_profile_edit_federates_to_mastodon"
)
def test_profile_edit_federates_from_mastodon(alice, plamenu_user, cli, db, marker):
    """Covers: inbound Update(Actor) — a Mastodon profile change refreshes
    our stored remote account, including the remote avatar URL."""
    alice_user, alice_domain = config.ALICE.split("@")
    new_name = f"Alice {marker}"

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    with step("alice edits her profile on Mastodon"):
        alice.update_profile(
            files={"avatar": ("avatar.png", tiny_png((30, 30, 200)), "image/png")},
            display_name=new_name,
        )

    with step("Plamenu applies the Update(Actor)"):
        wait_for(
            lambda: db.remote_display_name(alice_user, alice_domain) == new_name,
            desc="alice's new display name to reach Plamenu",
        )
        avatar = db.remote_avatar_url(alice_user, alice_domain)
        assert avatar and avatar.startswith("https://"), avatar


@pytest.mark.federation(
    direction="inbound", reverse_of="test_status_edit_federates_to_mastodon"
)
def test_status_edit_federates_from_mastodon(alice, plamenu_user, cli, db, marker):
    """Covers: inbound Update(Note) — a Mastodon status edit changes the
    stored content and records edited_at."""
    second_marker = unique("edited")

    with step(f"@{plamenu_user.username} follows {config.ALICE}"):
        cli.follow(plamenu_user.username, config.ALICE)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the outbound follow to be accepted",
        )

    with step("alice posts, and the status reaches Plamenu"):
        posted = alice.post_status(f"Original wording {marker}")
        status_id = wait_for(
            lambda: db.status_id_containing(marker),
            desc="the Mastodon post to arrive in Plamenu",
        )

    with step("alice edits the post; Plamenu applies the Update(Note)"):
        alice.edit_status(posted["id"], f"Corrected wording {second_marker}")
        wait_for(
            lambda: db.status_id_containing(second_marker) == status_id,
            desc="the edited content to replace the original",
        )
        assert db.status_edited_at(status_id) is not None
        assert db.status_id_containing(marker) is None, "old wording must be gone"
