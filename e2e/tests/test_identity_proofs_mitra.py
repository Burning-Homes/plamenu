"""FEP-c390 signatures published through each server's real client API."""

import base64
import json
import subprocess
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from plamenu_e2e import config
from plamenu_e2e.steps import step, wait_for

PLAMENU_PROOFS = "/api/v1/accounts/identity_statements"


def mitra_has(api, account_id, did):
    # Cached account endpoint: polling must not trigger a network refetch.
    return any(
        p["value"] == did.removeprefix("did:key:") and p["verified_at"]
        for p in api.account(account_id)["identity_proofs"]
    )


@pytest.mark.federation(
    direction="outbound", reverse_of="test_identity_proofs_from_mitra", peer="mitra"
)
def test_identity_proofs_to_mitra(mitra_erin, plamenu_user, plamenu_api, tmp_path):
    with step("sign a statement offline and publish it before Mitra fetches the actor"):
        actor = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        script = (
            Path(__file__).resolve().parents[2] / "scripts" / "sign-identity-proof.py"
        )
        result = subprocess.run(
            [
                "uv",
                "run",
                "--script",
                str(script),
                "--key",
                str(tmp_path / "identity.pem"),
                "--generate",
                "--actor",
                actor["id"],
            ],
            capture_output=True,
            text=True,
            check=True,
            timeout=120,
        )
        proof = json.loads(result.stdout)
        did = proof["subject"]
        assert plamenu_api._request("POST", PLAMENU_PROOFS, json=proof) == [proof]
        account = mitra_erin.resolve_account(plamenu_user.acct)
        assert account
        assert mitra_has(mitra_erin, account["id"], did), account

    with step("establish a follow so Mitra receives signed actor Updates"):
        mitra_erin.follow(account["id"])
        wait_for(
            lambda: mitra_erin.relationship(account["id"])["following"],
            desc="Mitra follow acceptance",
        )

    with step("remove the statement and observe Mitra's cached proof disappear"):
        plamenu_api._request("DELETE", PLAMENU_PROOFS, json={"subject": did})
        wait_for(
            lambda: not mitra_has(mitra_erin, account["id"], did),
            desc="proof removal delivered to Mitra",
        )

    with step("publish again and observe Mitra verify the pushed statement"):
        plamenu_api._request("POST", PLAMENU_PROOFS, json=proof)
        wait_for(
            lambda: mitra_has(mitra_erin, account["id"], did),
            desc="proof Update verified by Mitra",
        )
        local_actor = plamenu_api.ap_get(f"/users/{plamenu_user.username}")
        assert proof in local_actor["attachment"]
        plamenu_api._request("DELETE", PLAMENU_PROOFS, json={"subject": did})
        wait_for(
            lambda: not mitra_has(mitra_erin, account["id"], did),
            desc="final proof removal at Mitra",
        )


@pytest.mark.federation(
    direction="inbound", reverse_of="test_identity_proofs_to_mitra", peer="mitra"
)
def test_identity_proofs_from_mitra(mitra_erin, plamenu_user, plamenu_api, cli, db):
    with step("follow Erin from Plamenu so Mitra pushes profile Updates"):
        cli.follow(plamenu_user.username, f"erin@{config.MITRA_DOMAIN}")
        wait_for(
            lambda: db.outbound_follow_pending(plamenu_user.username) is False,
            desc="Plamenu follow acceptance",
        )
        account = plamenu_api.resolve_account(f"erin@{config.MITRA_DOMAIN}")
        assert account
        path = f"/api/v1/accounts/{account['id']}/identity_statements"

    with step("obtain Mitra's claim and sign it with a new user-owned key"):
        key = Ed25519PrivateKey.generate()
        public = base64.b64encode(
            b"Ed" + bytes(8) + key.public_key().public_bytes_raw()
        ).decode()
        claim = mitra_erin.get(
            "/api/v1/accounts/identity_claim",
            proof_type="minisign-unhashed",
            signer=public,
        )
        signature = base64.b64encode(
            b"Ed" + bytes(8) + key.sign(bytes.fromhex(claim["claim"]))
        ).decode()
        payload = {
            "proof_type": "minisign-unhashed",
            "did": claim["did"],
            "created_at": claim["created_at"],
            "signature": signature,
        }

    try:
        with step(
            "publish through Mitra and wait for Plamenu to verify the delivered proof"
        ):
            mitra_erin._request("POST", "/api/v1/accounts/identity_proof", json=payload)
            statements = wait_for(
                lambda: [
                    p for p in plamenu_api.get(path) if p["subject"] == claim["did"]
                ],
                desc="Mitra proof Update verified by Plamenu",
            )
            mitra_actor = mitra_erin.ap_get("/users/erin")
            original = next(
                p for p in mitra_actor["attachment"] if p.get("subject") == claim["did"]
            )
            assert statements == [original], (
                "The client API must expose the original signed statement"
            )
            assert original["proof"]["cryptosuite"] == "eddsa-jcs-2022"
    finally:
        mitra_erin._request(
            "DELETE", "/api/v1/accounts/identity_proof", json={"did": claim["did"]}
        )

    with step("Plamenu applies Mitra's proof-removal Update"):
        wait_for(
            lambda: (
                not any(p["subject"] == claim["did"] for p in plamenu_api.get(path))
            ),
            desc="proof removal delivered to Plamenu",
        )
