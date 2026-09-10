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
