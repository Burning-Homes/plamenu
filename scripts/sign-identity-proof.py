#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["cryptography>=49", "rfc8785>=0.1.4", "base58>=2.1.1"]
# ///
"""Sign an FEP-c390 statement locally. No HTTP requests or key uploads."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path

import base58
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
import rfc8785


def sign(key, actor_id, created=None):
    public = key.public_key().public_bytes_raw()
    multikey = "z" + base58.b58encode(b"\xed\x01" + public).decode()
    did = "did:key:" + multikey
    statement = {"type": "VerifiableIdentityStatement", "subject": did, "alsoKnownAs": actor_id}
    proof = {
        "type": "DataIntegrityProof",
        "cryptosuite": "eddsa-jcs-2022",
        "created": created or datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "verificationMethod": did + "#" + multikey,
        "proofPurpose": "assertionMethod",
    }
    data = hashlib.sha256(rfc8785.dumps(proof)).digest() + hashlib.sha256(rfc8785.dumps(statement)).digest()
    proof["proofValue"] = "z" + base58.b58encode(key.sign(data)).decode()
    return {**statement, "proof": proof}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key", type=Path, required=True, help="Local Ed25519 PEM private key")
    parser.add_argument("--generate", action="store_true", help="Create a new owner-only key file; fails if it exists")
    parser.add_argument("--actor", required=True, help="Exact actor ID from Settings → Identity proofs")
    args = parser.parse_args()
    if args.generate:
        key = Ed25519PrivateKey.generate()
        encoded = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())
        with os.fdopen(os.open(args.key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as stream:
            stream.write(encoded)
    else:
        key = serialization.load_pem_private_key(args.key.read_bytes(), password=None)
        if not isinstance(key, Ed25519PrivateKey):
            parser.error("The key must be Ed25519")
    print(json.dumps(sign(key, args.actor), indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
