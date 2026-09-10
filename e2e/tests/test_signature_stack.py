"""Modern signature stack, live against the peers.

`emit_integrity_proofs` defaults on, so every delivery carries an FEP-8b32
proof. RFC 9421 emission, however, is learned-not-guessed (approach a, see
plamenu/RFC9421_PLEROMA_FIX_PLAN.md): Plamenu defaults every delivery to
draft-cavage and only emits RFC 9421 to a peer proven to support it — a bare
outbound `200` is NOT proof (upstream Pleroma 200s an RFC 9421 inbox POST then
async-drops it). Support is learned from a positive capability signal: a
peer's FEP-844e `implements` advertisement (primary) or an inbound request the
peer itself signed with RFC 9421 (secondary).

This file covers the peers that get **draft-cavage**: Mastodon and Akkoma
neither advertise nor emit RFC 9421, so they earn no positive verdict and are
delivered to over cavage (the rest of the suite passing is the compatibility
net). It also asserts the FEP-521a key material on our own actor documents. The
peer that DOES get RFC 9421 (Mitra advertises it via FEP-844e) has its own
end-to-end coverage in test_mitra_rfc9421.py.
"""

import pytest
from plamenu_e2e import config, mastodon
from plamenu_e2e.api import Api
from plamenu_e2e.steps import log, step, wait_for


def _assert_rsa_mirror(doc: dict) -> None:
    """The RSA signing key must also appear as an `rsa-pub` Multikey under the
    exact id its `publicKey` block uses. Initial keys use `#main-key`; rotated
    keys append an immutable suffix while the old key remains advertised for
    the overlap window.

    Mastodon 4.7 resolves a signature's `keyId` against verification methods it
    builds from `assertionMethod`; builds between 2026-06-19 and 2026-07-06
    (mastodon#39725) let those *replace* `publicKey`, so an actor publishing
    only an Ed25519 method loses the RSA key its signatures name there. The
    mirror keeps `#main-key` resolvable under either rule."""
    matches = [
        method
        for method in doc["assertionMethod"]
        if method["id"] == doc["publicKey"]["id"]
    ]
    assert len(matches) == 1, matches
    mirror = matches[0]
    assert mirror["type"] == "Multikey"
    assert mirror["id"].startswith(f"{doc['id']}#main-key")
    assert mirror["controller"] == doc["id"]
    # multicodec rsa-pub (0x1205) always base58-encodes to a `z4MX` head.
    assert mirror["publicKeyMultibase"].startswith("z4MX")


def test_actor_documents_advertise_the_f2_stack(plamenu_user, db):
    plamenu = Api(config.PLAMENU_URL)

    with step("the user actor publishes the FEP-521a Ed25519 Multikey"):
        doc = plamenu.ap_get(f"/users/{plamenu_user.username}")
        methods = doc.get("assertionMethod")
        assert methods and len(methods) == 2, f"assertionMethod: {methods!r}"
        method = methods[0]
        assert method["type"] == "Multikey"
        assert method["id"] == f"{doc['id']}#ed25519-key"
        assert method["controller"] == doc["id"]
        assert method["publicKeyMultibase"].startswith("z6Mk")
        stored = db.ed25519_public_key(plamenu_user.username, None)
        assert stored == method["publicKeyMultibase"], (
            "the published Multikey must be the stored one"
        )
        # The RSA key stays alongside for draft-cavage verifiers.
        assert "BEGIN PUBLIC KEY" in doc["publicKey"]["publicKeyPem"]
        _assert_rsa_mirror(doc)

    with step("the instance actor publishes one too"):
        instance = plamenu.ap_get("/actor")
        assert instance["assertionMethod"][0]["id"] == f"{instance['id']}#ed25519-key"
        assert instance["assertionMethod"][0]["publicKeyMultibase"].startswith("z6Mk")
        _assert_rsa_mirror(instance)

    with step("FEP-844e advertises the signature capabilities"):
        hrefs = [entry["href"] for entry in instance["implements"]]
        for expected in (
            "https://w3id.org/fep/521a",
            "https://w3id.org/fep/8b32",
            "https://datatracker.ietf.org/doc/html/rfc9421",
        ):
            assert expected in hrefs, f"{expected} missing from {hrefs!r}"


def _follow_from_plamenu(plamenu_api, acct: str) -> dict:
    """Drives one high-value delivery (a Follow) toward `acct`'s host."""
    account = plamenu_api.resolve_account(acct)
    assert account, f"Plamenu cannot resolve {acct}"
    plamenu_api.follow(account["id"])
    return account


@pytest.mark.federation(
    direction="outbound",
    peer="mastodon",
    one_way_reason="outbound signature default: Plamenu delivers to Mastodon over draft-cavage (approach a) and never false-learns RFC 9421 from a 200; inbound verification is implicit in every signed test, so there is no separate reverse.",
)
def test_mastodon_delivery_defaults_to_cavage(plamenu_user, plamenu_api, alice, db):
    """Approach (a): Plamenu defaults to draft-cavage and only emits RFC 9421 to
    a peer proven — by that peer's own inbound RFC 9421 — to speak it. Mastodon
    signs us with draft-cavage, so it never earns a positive row; yet the
    proof-carrying, cavage-signed Follow still delivers and processes over
    there. The assertion that NO rfc9421 row is learned for Mastodon (despite
    its 200s) is the regression guard for the black-hole fix — a 200 must never
    mint a positive verdict."""
    with step(f"alice resolves @{plamenu_user.acct} first"):
        # Resolving before we follow keeps Mastodon's account creation out
        # of a race with its own inbox processing of our Follow (its
        # ResolveAccountService 422s "Username has already been taken" when
        # web and sidekiq insert the same account concurrently).
        remote = alice.resolve_account(plamenu_user.acct)
        assert remote, "Mastodon could not resolve our user"

    with step("follow alice (one signed delivery to mastodon)"):
        _follow_from_plamenu(plamenu_api, config.ALICE)
        wait_for(
            lambda: mastodon.remote_follows_local(
                plamenu_user.username,
                config.PLAMENU_DOMAIN,
                "alice",
            ),
            desc="Mastodon to commit the proof-carrying, cavage-signed Follow",
        )

    with step("no RFC 9421 verdict is learned for mastodon (cavage default)"):
        # Mastodon does not sign us with RFC 9421, so nothing is learned; the
        # outbound 200 must never be mistaken for RFC 9421 support.
        assert db.rfc9421_pref(config.MASTODON_DOMAIN) is None, (
            "a bare outbound 200 must not mint an rfc9421 row "
            f"(got {db.rfc9421_pref(config.MASTODON_DOMAIN)!r})"
        )
        log("no false-positive rfc9421 row for mastodon.local, delivery landed")


@pytest.mark.federation(
    direction="outbound",
    peer="pleroma",
    one_way_reason="outbound signature default: Plamenu delivers to Akkoma over draft-cavage (approach a); inbound verification is implicit in every signed test, so there is no separate reverse.",
)
def test_cavage_delivery_lands_and_learns_nothing(
    plamenu_user, plamenu_api, pleroma_bob, db
):
    """Akkoma reads only the draft-cavage `Signature` header. Under approach (a)
    Plamenu never knocks RFC 9421 at an unknown host, so the cavage Follow
    delivers directly (no rejected knock) and — because Akkoma signs us with
    cavage too — no positive verdict is learned."""
    bob = f"bob@{config.PLEROMA_DOMAIN}"
    with step("follow bob (one signed delivery to pleroma/akkoma)"):
        _follow_from_plamenu(plamenu_api, bob)
        account = plamenu_api.lookup(bob)
        wait_for(
            lambda: plamenu_api.relationship(account["id"])["following"],
            desc="bob's Accept to arrive (the cavage delivery worked)",
        )
    with step("no RFC 9421 verdict is learned for pleroma.local (cavage default)"):
        assert db.rfc9421_pref(config.PLEROMA_DOMAIN) is None, (
            f"expected no rfc9421 row for pleroma.local, got {db.rfc9421_pref(config.PLEROMA_DOMAIN)!r}"
        )
