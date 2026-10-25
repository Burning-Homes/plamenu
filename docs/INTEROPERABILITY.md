# Interoperability testing

Rust integration tests in `crates/server/tests/` exercise the router, database,
delivery queue, signatures, and serialization using pinned ActivityPub fixtures.
The Python suite in `e2e/` tests HTTPS exchanges with running peer servers.

The Python E2E suite is currently tied to one developer's machine and an ad hoc
local setup. It is not portable or expected to be usable elsewhere as provided;
it is included for transparency and as examples. For release builds, the suite is
omitted by default (`./dev release --skip-e2e` makes that explicit); the
portable application tests and packaged-image restore drill still run.

## Developer's live-suite workflow

The existing machine-specific workflow runs from the source checkout:

```sh
./dev provision-peers
./dev up
./dev e2e-full
```

Provisioning uses the Compose fixtures and upstream revisions in `e2e/peers/`.
It prepares local TLS and starts the required peer fleet. Generated checkouts,
credentials, certificates, and persistent state are ignored. To use an existing
fleet, configure the peer paths and URLs in `.env.example`.

Keep the generated matrix with release evidence. It records outcomes, skips,
expected failures, checkout differences, and running image IDs. A passing test
establishes the exchange it asserts for that peer configuration. Skipped cases
remain unverified. The source checkout's `e2e/README.md` lists the release policy
and report format.

## Identity proofs with Mitra

`./dev e2e tests/test_identity_proofs_mitra.py -v` checks FEP-c390 Ed25519
proofs through both servers' client APIs. It covers initial discovery at Mitra,
verified actor Updates and removal in both directions, and preservation of
Mitra's original signed statement. Update checks read cached accounts.

## Exchanges between Plamenu servers

`e2e/tests/test_plamenu2_*.py` uses a second instance at
`https://plamenu2.local`. The test session creates it from the working tree with
its own database, media directory, and port, then tears it down.
`e2e/plamenu_e2e/ephemeral.py` manages this instance and the self-destruct peer.

These tests cover receiving Plamenu's own quotes, reactions, group moderation,
events, articles, collections, and follower migration. They check that the two
ends agree; foreign-peer tests are still needed to check interoperability.

## Unicode account handles

Remote usernames retain their Unicode spelling. Domains use IDNA ASCII for
account identity, API `acct` fields, federation and profile paths; the native
client shows Unicode domains in labels, reply drafts, autocomplete and account
management forms.

Discovery follows [RFC 7565](https://www.rfc-editor.org/rfc/rfc7565): an `acct:`
URI percent-encodes the UTF-8 username, and WebFinger separately encodes that
URI as its `resource` query parameter. Incoming subjects decode the username
once before validation. Malformed UTF-8, encoded separators and double-encoded
usernames are rejected. The actor URI must still match the WebFinger result.

The federation transport test exercises an actual HTTP/JRD response containing
an encoded Chinese username. Server-router tests cover Application actors,
video channels on IDNs, mentions in all post formats, search, profile links,
and no-script reply/preview and account-management forms.

A successful WebFinger lookup is insufficient if the advertised actor URL
returns HTML instead of an ActivityPub document. Check its Content-Type and
canonical identity separately from IDN parsing; a normal website or REST channel
response does not authenticate an actor.

## BookWyrm

The local fixture pins unmodified BookWyrm 0.9.3 and its official prebuilt
image. `e2e/tests/test_bookwyrm_federation.py` covers WebFinger discovery,
Follow/Accept/Undo in both directions, published book reviews, Plamenu Notes
mentioning a BookWyrm user, replies in both directions, and resolving reviews,
ratings, comments and quotations. Social actions use only fixture accounts.
The lifecycle cases also check edits and deletion in both directions.

BookWyrm signs profile discovery with an instance actor whose key ID is
`<actor>/#main-key`, while its canonical actor ID has no final slash. Plamenu
resolves the key's base URL through canonical-object discovery, independently
fetches that canonical actor, and verifies ownership of the advertised key and
the signature. It caches the canonical identity rather than a slash alias.
This also permits BookWyrm to verify Plamenu's incoming Follow before accepting
it. Secure mode and signature checks remain enabled in the live test.
An upgrade migration clears only the old trailing-slash identity-mismatch
failure records, including exhausted budgets, so existing installations can
retry without resetting unrelated federation failures.

BookWyrm intentionally ignores unrelated microblog Notes. Mentions of local
users and replies to known posts are accepted; an ordinary public Note from a
followed Plamenu user is not guaranteed to appear there. Its non-BookWyrm wire
format uses standard Article/Note objects: reviews retain text plus a title
containing the book and rating, while comments, quotations and ratings include
readable book context. Covers and generated preview images can accompany that
text; Plamenu does not need to identify itself as BookWyrm to receive it.

BookWyrm sends edits as native Review/Comment/Quotation objects even when the
original Create used the standard format. For an existing post owned by the
authenticated sender, Plamenu fetches its canonical standard representation
and applies the ordinary author, edit-order and storage checks. Unknown posts,
foreign owners and invalid fallback documents cannot use this path to create
or replace content. This keeps book context and revised ratings readable.
If the first fetch predates BookWyrm's publication-transaction commit, bounded
retries require the fetched revision to reach the announced `updated` stamp.
A persistently stale snapshot is rejected rather than acknowledged as applied.

Its native vocabulary additionally has Review, Rating, Comment, Quotation and
GeneratedNote, plus Work, Edition, Author, shelves and reading lists. Plamenu
does not advertise BookWyrm-native support or treat catalogue objects as posts.
A native adapter would need to preserve book identity, rating, quotation and
reading progress explicitly; merely adding these type names to the ingest
allowlist would lose that meaning. The tested standard format supports basic
discussion without adding a reading application.

BookWyrm also speaks standard Like, Announce and Undo, plus relationship Block,
Reject and Move activities. Those map to existing Plamenu handlers; the new live
fixture does not establish their BookWyrm-specific behavior. Catalogue and
shelf/list activities need their own semantics and are not converted into feed
posts or used to advertise full BookWyrm compatibility.

BookWyrm 0.9.3 serves its outbox as plain `application/json`, unlike its actor
and individual status endpoints, which use `application/activity+json`.
Plamenu's ActivityPub fetcher requires an ActivityPub media type, so automatic
outbox history import is limited by that upstream response. New inbox deliveries
and individual status resolution are separate paths covered by the live tests.

## Check content presentation

For articles, events, media, and other structured posts, inspect both the
received ActivityPub object and the remote client's presentation. Successful
delivery does not establish that the title, body, attachments, or controls are
usable. [Protocol details](federation/protocol.md) describes the wire format.

## Report a federation problem

Include both software versions, the affected actor/object URLs where safe,
delivery direction, redacted request/response headers and ActivityPub JSON,
and whether authorized fetch is enabled. State whether the object arrived
through an inbox or was fetched by URL. Remove tokens, cookies, private content,
and signing keys before sharing evidence.
