"""Signed ActivityPub GETs, for introspecting Plamenu under authorized fetch.

Secure mode (the product default) requires server-to-server GETs of object
routes — statuses, collections, followers/following — to carry a valid HTTP
signature, exactly as Mastodon's `AUTHORIZED_FETCH` does. The e2e harness has
no federated identity of its own, so it borrows a real one: alice@mastodon.local,
whose RSA key we read from the Mastodon test database (the same trick
`redeliver.py` uses to sign POST replays). Plamenu dereferences alice's actor to
verify the signature, so this exercises the genuine peer-fetch path rather than
a bypass.
"""

import base64
import functools
import json
import re
from email.utils import formatdate
from urllib.parse import urlsplit

import requests
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

from . import config, shell

AP = "application/activity+json"


# Mastodon 4.7 moved local account keys out of `accounts.private_key` into the
# `keypairs` table, where the column is ActiveRecord-encrypted and so cannot be
# read with psql. Ask Rails for it instead, keeping the pre-4.7 column as a
# fallback so the harness stays usable against older peers.
_PRIVATE_KEY_RUNNER = """
account = Account.find_by(username: __USERNAME__, domain: nil)
keypair = account.keypairs.where(revoked: false).order(:created_at).first if account.respond_to?(:keypairs)
fragment = keypair&.local_fragment || '#main-key'
private_key = keypair&.private_key
private_key ||= account.private_key if account.respond_to?(:private_key)
puts("PLAMENU_E2E_KEY_FRAGMENT=#{fragment}")
puts(private_key)
"""


@functools.cache
def mastodon_signer(username: str = "alice") -> tuple[str, str]:
    """`(keyId, private_key_pem)` for a local Mastodon actor, cached per user.

    Both halves are version-agnostic: the actor URI comes from webfinger, the
    private key from a Rails runner that reads 4.7's active `keypairs` row and falls
    back to the legacy `accounts.private_key` column."""
    wf = requests.get(
        f"{config.MASTODON_URL}/.well-known/webfinger",
        params={"resource": f"acct:{username}@{config.MASTODON_DOMAIN}"},
        verify=False,
        timeout=30,
    )
    wf.raise_for_status()
    actor_uri = next(
        link["href"] for link in wf.json()["links"] if link.get("rel") == "self"
    )
    out = shell.masto_compose(
        "exec",
        "-T",
        "web",
        "bin/rails",
        "runner",
        _PRIVATE_KEY_RUNNER.replace("__USERNAME__", json.dumps(username)),
    )
    # Rails may print boot noise around the key; keep the PEM block alone.
    match = re.search(
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
        out,
        re.DOTALL,
    )
    assert match, f"no private key for {username} in the Mastodon DB: {out[:200]!r}"
    fragment = re.search(r"^PLAMENU_E2E_KEY_FRAGMENT=(#[^\s]+)$", out, re.MULTILINE)
    assert fragment, f"no key fragment for {username} in the Mastodon DB: {out[:200]!r}"
    return actor_uri + fragment.group(1), match.group(0) + "\n"


def signed_ap_get(url: str) -> requests.Response:
    """GET `url` as a signed server-to-server AP fetch.

    Signs the draft-cavage header set Plamenu's `from_get_request` requires
    (`(request-target) host date accept`) — byte-for-byte the format
    `RequestSigner::sign_get` produces on the Rust side."""
    parts = urlsplit(url)
    host = parts.netloc
    path_and_query = parts.path + (f"?{parts.query}" if parts.query else "")
    key_id, pem = mastodon_signer()
    date = formatdate(usegmt=True)
    signing_string = (
        f"(request-target): get {path_and_query}\n"
        f"host: {host}\ndate: {date}\naccept: {AP}"
    )
    key = serialization.load_pem_private_key(pem.encode(), password=None)
    signature = base64.b64encode(
        key.sign(signing_string.encode(), padding.PKCS1v15(), hashes.SHA256())
    ).decode()
    return requests.get(
        url,
        headers={
            "Host": host,
            "Date": date,
            "Accept": AP,
            "Signature": (
                f'keyId="{key_id}",algorithm="rsa-sha256",'
                f'headers="(request-target) host date accept",'
                f'signature="{signature}"'
            ),
        },
        verify=False,
        timeout=30,
    )
