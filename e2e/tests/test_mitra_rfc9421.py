"""RFC 9421 signature path, exercised end-to-end against real Mitra.

RFC 9421 emission is rare-by-design after the black-hole fix (approach a): no
common peer *emits* 9421, so it is easy for the whole 9421 code path to rot
untested. Mitra is the exception worth pinning down — it *advertises* RFC 9421
via FEP-844e `implements` and *verifies* it, even though it *emits* draft-cavage
by default. These two tests prove the 9421 path actually works in both
directions against that real peer:

* Outbound — Plamenu learns Mitra's capability from the FEP-844e `implements`
  on its fetched actor, emits a 9421-signed Follow, and Mitra verifies it (the
  per-host verdict is not downgraded).
* Inbound — Mitra's `send-activity --rfc9421` CLI (the only way to make Mitra
  emit 9421, since its deliverer hardcodes cavage) signs a Like that Plamenu
  verifies and processes.

Background: plamenu/RFC9421_PLEROMA_FIX_PLAN.md and the `signature_prefs`
module docs. Optional peer: skips when Mitra isn't running.
"""

import pytest
from plamenu_e2e import config, mitra
from plamenu_e2e.steps import step, wait_for

ERIN = f"erin@{config.MITRA_DOMAIN}"
ERIN_URI = f"{config.MITRA_URL}/users/erin"


@pytest.mark.federation(
    direction="outbound",
    peer="mitra",
    reverse_of="test_mitra_emits_verified_rfc9421_to_plamenu",
)
def test_plamenu_emits_verified_rfc9421_to_mitra(
    mitra_erin, plamenu_user, plamenu_api, cli, db
):
    """Plamenu learns Mitra speaks RFC 9421 from its FEP-844e `implements`
    advertisement (declarative capability, not a delivery `200`), emits a
    9421-signed Follow, and Mitra verifies it."""
    with step("force a fresh fetch of erin so FEP-844e capability learning runs"):
        # A persisted dev DB may already hold erin from before this feature; a
        # cached actor is not re-parsed, so drop it to re-trigger the learning.
        db.forget_remote_account(ERIN_URI)
        account = plamenu_api.resolve_account(ERIN)
        assert account, "Plamenu could not resolve erin"

    with step("Plamenu recorded the rfc9421 capability from erin's actor"):
        assert db.rfc9421_pref(config.MITRA_DOMAIN) is True, (
            "erin advertises rfc9421 in its FEP-844e implements; Plamenu must "
            f"record the capability (got {db.rfc9421_pref(config.MITRA_DOMAIN)!r})"
        )

    with step("a 9421-signed Follow is delivered to Mitra and accepted there"):
        cli.follow(plamenu_user.username, ERIN)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="Mitra to Accept our RFC 9421-signed Follow",
        )

    with step("the knock was not downgraded — Mitra verified our 9421 signature"):
        # A hard 9421 refusal would fall back to cavage and record `false`
        # (delivery.rs downgrade). It stayed `true`, so the 9421 knock got a 2xx
        # and Mitra verified our signature — the outbound 9421 path works.
        assert db.rfc9421_pref(config.MITRA_DOMAIN) is True, (
            "the rfc9421 verdict was downgraded, so Mitra rejected our 9421 "
            "signature and we fell back to cavage"
        )


@pytest.mark.federation(
    direction="inbound",
    peer="mitra",
    reverse_of="test_plamenu_emits_verified_rfc9421_to_mitra",
)
def test_mitra_emits_verified_rfc9421_to_plamenu(
    mitra_erin, plamenu_user, plamenu_api, cli, db
):
    """Mitra signs a Like with RFC 9421 (via its `send-activity --rfc9421` CLI,
    the only 9421-emission path), delivers it to Plamenu's inbox, and Plamenu
    verifies and processes it into a favourite."""
    with step("the plamenu user follows erin so Mitra knows it (delivery recipient)"):
        # send-activity resolves the recipient from Mitra's stored profiles, so
        # the plamenu actor must be known to Mitra first; the Follow does that.
        cli.follow(plamenu_user.username, ERIN)
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="the follow to erin to be accepted (Mitra learns the plamenu actor)",
        )

    with step("the plamenu user posts a public status"):
        status = plamenu_api.post_status(
            "like me from mitra over rfc 9421", visibility="public"
        )

    with step("Mitra sends a 9421-signed Like; Plamenu verifies and processes it"):
        recipient = plamenu_api.ap_get(f"/users/{plamenu_user.username}")["id"]
        result = mitra.send_activity_rfc9421(
            mitra.like_activity(status["uri"]), recipient
        )
        # Plamenu answers 401 to an unverifiable signature, so a 202 here is
        # proof the RFC 9421 signature verified.
        assert "202" in result, (
            f"Mitra's 9421-signed delivery was not accepted: {result!r}"
        )
        wait_for(
            lambda: db.favourite_count(int(status["id"])) == 1,
            desc="the 9421-signed favourite from erin to land on the plamenu status",
        )

    with step("the verified inbound 9421 request also recorded Mitra's capability"):
        assert db.rfc9421_pref(config.MITRA_DOMAIN) is True
