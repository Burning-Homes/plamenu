"""Replaying already-delivered activities against Plamenu's inbox.

Motivated by the 2026-07 Pleroma incident: a peer re-sent roughly a full day
of inbound activities and, before the redelivery-idempotency fixes, every one
of them re-notified and resurfaced day-old posts on live timelines. A peer
cannot be coaxed into resending on demand, so these helpers rebuild an
activity the peer already delivered, sign it with the *real* actor's key
(extracted from the peer's database) and POST it straight to Plamenu's
shared inbox — byte-for-byte the same verification path as a genuine
redelivery.
"""

import base64
import hashlib
import json
from email.utils import formatdate

import requests
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

from . import config, shell

AS2_CONTEXT = "https://www.w3.org/ns/activitystreams"
PUBLIC = f"{AS2_CONTEXT}#Public"


def pleroma_private_key_pem(nickname: str) -> str:
    """The RSA private key PEM of a local user, out of the peer's database — so
    replays carry a genuine signature that Plamenu resolves to the real actor.

    The peer runs Akkoma, which stores actor keys in a dedicated `signing_keys`
    table (joined to `users`) rather than the old Pleroma `users.keys` column."""
    pem = shell.pleroma_compose(
        "exec",
        "-T",
        "db",
        "psql",
        "-U",
        "pleroma",
        "-d",
        "pleroma",
        "-tA",
        "-c",
        "SELECT sk.private_key FROM signing_keys sk"
        " JOIN users u ON u.id = sk.user_id"
        f" WHERE u.nickname = '{nickname}' AND u.local",
    ).strip()
    assert "PRIVATE KEY" in pem, f"no signing key for {nickname}: {pem[:120]!r}"
    return pem


def fetch_ap(uri: str) -> dict:
    """Fetch a public ActivityPub object anonymously (the disposable Pleroma
    does not require signed fetches)."""
    r = requests.get(
        uri,
        headers={"Accept": "application/activity+json"},
        verify=False,
        timeout=30,
    )
    r.raise_for_status()
    return r.json()


def deliver(activity: dict, *, actor_uri: str, private_key_pem: str) -> int:
    """Sign `activity` as `actor_uri` (cavage HTTP signature over the header
    set Mastodon and Pleroma both sign: `(request-target) host date digest`)
    and POST it to Plamenu's shared inbox. Returns the HTTP status code —
    202 whether the activity was fresh or a duplicate; the assertions about
    what it *did* belong to the caller."""
    body = json.dumps(activity).encode()
    date = formatdate(usegmt=True)
    digest = "SHA-256=" + base64.b64encode(hashlib.sha256(body).digest()).decode()
    signing_string = (
        f"(request-target): post /inbox\n"
        f"host: {config.PLAMENU_DOMAIN}\ndate: {date}\ndigest: {digest}"
    )
    key = serialization.load_pem_private_key(private_key_pem.encode(), password=None)
    signature = base64.b64encode(
        key.sign(signing_string.encode(), padding.PKCS1v15(), hashes.SHA256())
    ).decode()
    r = requests.post(
        f"{config.PLAMENU_URL}/inbox",
        data=body,
        headers={
            "Date": date,
            "Digest": digest,
            "Signature": (
                f'keyId="{actor_uri}#main-key",algorithm="rsa-sha256",'
                f'headers="(request-target) host date digest",'
                f'signature="{signature}"'
            ),
            "Content-Type": "application/activity+json",
        },
        verify=False,
        timeout=30,
    )
    return r.status_code


def create_envelope(note: dict, *, actor_uri: str, suffix: str = "replay") -> dict:
    """A `Create` wrapping an already-federated note, the way its origin
    server would resend it (fresh activity id; the object is what dedup
    must key on)."""
    return {
        "@context": AS2_CONTEXT,
        "id": f"{note['id']}#{suffix}-create",
        "type": "Create",
        "actor": actor_uri,
        "to": note.get("to", [PUBLIC]),
        "cc": note.get("cc", []),
        "object": note,
    }


def update_envelope(note: dict, *, actor_uri: str, suffix: str = "replay") -> dict:
    """An `Update` carrying a (possibly stale) revision of a note."""
    return {
        "@context": AS2_CONTEXT,
        "id": f"{note['id']}#{suffix}-update",
        "type": "Update",
        "actor": actor_uri,
        "to": note.get("to", [PUBLIC]),
        "cc": note.get("cc", []),
        "object": note,
    }
