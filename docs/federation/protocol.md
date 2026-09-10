# ActivityPub protocol and extensions

Plamenu uses WebFinger and NodeInfo for discovery, ActivityPub actor and object
endpoints, signed inbox delivery, and signed or authorized fetch.

## Identity and transport

Local actor and object URLs use the configured `domain`, which must stay fixed
after federation begins. An optional `account_domain` provides shorter handles;
its WebFinger, host-meta, and NodeInfo discovery paths must forward to Plamenu.
Remote handle changes are accepted when WebFinger resolves to the same actor URI.

Transport supports draft-cavage and RFC 9421 HTTP signatures, FEP-8b32 integrity
proofs, delivery retries, and URL/IP checks on remote fetches. Remote RSA,
Ed25519, and ML-DSA-44 verification keys are stored per actor. Local RSA and
Ed25519 private keys are encrypted at rest.

See [signing-key operations](../FEDERATION_KEY_OPERATIONS.md) for rotation and
recovery, [queues](../QUEUES.md) for retry behavior, and
[Tor/I2P transport](../TOR_TRANSPORT.md) for optional proxy routing.

## Identity proofs

[FEP-c390](https://w3id.org/fep/c390) statements are published in actor
attachments and verified on remote actor fetches and Updates. Plamenu supports
Ed25519 `did:key` with `eddsa-jcs-2022` and its legacy suite name. It retains
valid original statements and discards invalid or removed ones. Expired proofs
are excluded when profiles or actors are read. This remains an experimental
FEP; Ethereum and prehashed Minisign proofs are not implemented.

See [member controls](../members/account-and-safety.md#identity-proofs-experimental) and
[client endpoints](../api/index.md#identity-statements). Identity keys are
user-owned and separate from the server's federation signing keys.

## Content

Local publishing includes Notes, polls, Articles, Events, and group threads.
Titled ordinary group posts use `Page`. Replies, quotes, favourites, boosts,
emoji reactions, group moderation, and event participation have federation
handlers. Groups use experimental FEP-1b12/Lemmy-oriented behavior.

Group actors publish `postingPolicy` in the
`https://codefloe.com/plamenu/plamenu/ns#` namespace: `anyone`, `members`, or
`mods`. Lemmy's `postingRestrictedToMods` is also emitted and is true for `mods`.

Outgoing Articles put the title in `name` and a heading in the body, supporting
readers that display only the body. `summary` carries the content warning.
Plamenu removes a matching leading title when rendering incoming articles.

Inbound object handling includes Note, Question, Article, Page, Event,
Document, and Video, with remote media handling for audio/video and PeerTube
streams. These are receiving and rendering capabilities; they do not imply
that Plamenu provides a publishing tool for each remote object or media type.
The [posting guide](../members/posting.md) describes local web controls.

Remote software controls its own presentation. Some clients display an article
or event as a title and link. Fixture and live-peer coverage is described in
[interoperability testing](../INTEROPERABILITY.md).

## Webxdc

The experimental [FEP-752d](../fep/fep-752d.md) implementation supports
Note invitations, `Group`/`WebxdcSession` actors, membership, sequenced updates,
fan-out, replay, participant removal, session closure, and deletion.

Bundles are publicly retrievable. Plamenu accepts `application/webxdc+zip` and
legacy `application/x-webxdc`, checks the SHA-256 Multihash in `digestMultibase`,
and validates archive limits before caching or execution. Serve bundles from
the separate Webxdc origin described in the deployment configuration.

`window.webxdc.joinRealtimeChannel()` uses the draft's ephemeral ActivityPub
baseline: binary packets travel as `Create`/`WebxdcEphemeral` and coordinator
`Announce` activities. Connected clients receive them over a host WebSocket.
Packets have no durable serial, database storage, retry job, or replay, and
expire within five seconds. This does not provide a low-latency guarantee.

Plamenu uses bounded in-memory fan-out, duplicate suppression, and delivery
concurrency. Routing/authorization metadata is cached for two seconds and signing
keys for thirty seconds; local Webxdc membership and lifecycle changes invalidate
authorization immediately. Signed inbox requests still read known actor keys and
instance policy. Packet processing skips durable inbox bookkeeping and unknown
key refetch. Packet payloads never pass through the database notification bus.
Run one serving process per instance for this experimental channel; replicas
would need a shared, non-persistent fan-out transport. Reverse proxies should
also exclude ephemeral inbox bodies from request logging.
