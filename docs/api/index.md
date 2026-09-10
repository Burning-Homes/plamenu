# Clients and APIs

Plamenu provides a Mastodon-compatible REST API, OAuth 2, WebSocket streaming,
Web Push, and an experimental Lemmy `/api/v3` adapter.

## Sign in from an app

Enter your server's HTTPS hosting domain in a Mastodon-compatible client. The
app opens Plamenu's authorization page, where you sign in and approve access.
On a split-domain installation, use the hosting domain (for example,
`social.example.com`), even if your handle ends in `@example.com`.

## Build a client

- [OAuth and authentication](authentication.md)
- [Lemmy API](../LEMMY_API_COMPATIBILITY.md)

Test authentication, timelines, posting, uploads, and notifications against the
exact Plamenu version you intend to support. Plamenu-specific APIs may change
incompatibly during the experimental release series.

## Identity statements

FEP-c390 uses signed statements to link an actor to a user-owned key. These
Plamenu endpoints are separate from the retired Mastodon Keybase API:

- `GET /api/v1/accounts/:id/identity_statements`: original verified JSON statements.
- `POST /api/v1/accounts/identity_statements`: publish a signed statement as the JSON body.
- `DELETE /api/v1/accounts/identity_statements`: remove one with `{"subject":"did:key:…"}`.

Writes require OAuth `write:accounts` and return the remaining statements.
Publishing replaces a statement with the same subject; up to ten are allowed,
with a 16 KiB limit per document. Invalid statements return 422. Both writes
send an actor Update to connected peers.

Supported proofs use Ed25519 `did:key` subjects, `DataIntegrityProof`,
`assertionMethod`, and `eddsa-jcs-2022` (also its legacy name `jcs-eddsa-2022`).
The verification method must be the subject DID or its canonical `#z6Mk…`
fragment. The statement must name the exact actor ID in `alsoKnownAs`.
Expired proofs are excluded from responses. Ethereum and legacy prehashed
Minisign proofs are not supported.
