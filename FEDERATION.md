# Federation

## Supported protocols and standards

- [ActivityPub](https://www.w3.org/TR/activitypub/) — server-to-server federation.
- [ActivityStreams 2.0](https://www.w3.org/TR/activitystreams-core/) and its
  [vocabulary](https://www.w3.org/TR/activitystreams-vocabulary/).
- [WebFinger](https://www.rfc-editor.org/rfc/rfc7033) — account discovery.
- [NodeInfo](https://nodeinfo.diaspora.software/) — schemas 2.0 and 2.1.
- [HTTP Signatures (draft-cavage)](https://datatracker.ietf.org/doc/html/draft-cavage-http-signatures) — RSA-SHA256.
- [HTTP Message Signatures (RFC 9421)](https://www.rfc-editor.org/rfc/rfc9421) — RSA and Ed25519.

## Supported FEPs

| FEP | Support |
| --- | --- |
| [0151: NodeInfo in Fediverse Software](https://w3id.org/fep/0151) | Instance metadata and usage statistics. |
| [03c1: Actors without acct-URI](https://w3id.org/fep/03c1) | Partial: imports actors without a WebFinger address and displays a generated handle. The FEP specifies displaying the actor URI. |
| [044f: Consent-respecting quote posts](https://w3id.org/fep/044f) | Quote requests, authorizations, policies, and revocation. |
| [171b: Conversation Containers](https://w3id.org/fep/171b) | Receives container activities. Hosting private conversation containers is optional and off by default. |
| [1b12: Group federation](https://w3id.org/fep/1b12) | Experimental group posts, announcements, membership, and moderation. |
| [2c59: Discovery of a WebFinger address from an ActivityPub actor](https://w3id.org/fep/2c59) | Receive only: reads the remote actor's `webfinger` property. |
| [3b86: Activity Intents](https://w3id.org/fep/3b86) | Advertises `Create` and `Object` intents through WebFinger. |
| [5219: Groups and permissions](https://w3id.org/fep/5219) | Partial: publishes and reads group moderator affiliations. |
| [521a: Representing actor's public keys](https://w3id.org/fep/521a) | Publishes RSA and Ed25519 Multikeys; resolves inline and referenced remote verification methods. |
| [67ff: FEDERATION.md](https://w3id.org/fep/67ff) | This document. |
| [752d: Federated Webxdc application sessions](docs/fep/fep-752d.md) | Repository-local draft: session actors, invitations, membership, durable updates and replay, ephemeral packets, and session lifecycle. |
| [7888: Demystifying the context property](https://w3id.org/fep/7888) | Uses `context` to group posts into conversations. |
| [7aa9: Featuring recommendations using a dedicated collection](https://w3id.org/fep/7aa9) | Account collections with inclusion requests, authorizations, and revocation. |
| [844e: Capability discovery](https://w3id.org/fep/844e) | Advertises `implements` and reads remote capabilities, including RFC 9421 support. |
| [8b32: Object Integrity Proofs](https://w3id.org/fep/8b32) | Signs with `eddsa-jcs-2022` when enabled; verifies that suite, legacy `jcs-eddsa-2022`, and `mldsa44-jcs-2024`. |
| [8fcf: Followers collection synchronization across servers](https://w3id.org/fep/8fcf) | Domain-scoped follower collections and `Collection-Synchronization` headers. |
| [ae97: Client-side activity signing](https://w3id.org/fep/ae97) | Gateway for client-signed actors and activities, including Minimitra's media API. |
| [c390: Identity Proofs](https://w3id.org/fep/c390) | Experimental actor attachments using Ed25519 `did:key` and `eddsa-jcs-2022` or its legacy suite name. Ethereum and prehashed Minisign proofs are unsupported. |
| [e232: Object Links](https://w3id.org/fep/e232) | Emits and reads object links for quotes. |
| [ef61: Portable Objects](https://w3id.org/fep/ef61) | Partial: gateway support for Minimitra-compatible portable identifiers using Ed25519 `did:key`. |
| [f228: Backfilling conversations](https://w3id.org/fep/f228) | Serves public conversation collections and fetches remote conversation history. |

## ActivityPub behavior

### Actors and discovery

Local accounts use `Person`, bots use `Service`, communities use `Group`, and
the instance actor uses `Application`. Webxdc sessions use both `Group` and `WebxdcSession`.
Actors advertise their inboxes and collections; the shared inbox is `/inbox`.

WebFinger accepts `acct:user@domain`, `user@domain`, and `@user@domain` for
accounts. It does not accept account actor URLs as the resource. The instance
actor has separate discovery through the bare domain, `domain@domain`, or its
`/actor` URL. An optional `account_domain` changes handles while actor IDs stay
on `domain`.

### Activities and content

Plamenu sends and receives follows and their responses, post creation, edits
and deletion, likes, boosts, account moves, and event participation. Quotes
use `QuoteRequest` and `QuoteAuthorization`; account collection inclusion uses
`FeatureRequest` and `FeatureAuthorization`.

| Content | Send | Receive |
| --- | --- | --- |
| `Note` | Ordinary posts and replies | Yes |
| `Question` | Polls | Yes |
| `Article` | Articles | Yes |
| `Event` | Events | Yes |
| `Page` | Titled group discussions | Yes |
| `Document`, `Image`, `Audio`, `Video` as posts | No | Yes |

Reactions are sent as `EmojiReact`. Incoming reactions also accept
`EmojiReaction` and `Like` with emoji content. Remote HTML is sanitized;
inline images and embedded players are removed from `content`. Inline images
are recovered as attachments when the post has no other usable media.

Groups distribute accepted activities through `Announce`. Forwarded activities
are authenticated using the author's integrity proof or by fetching from the
origin. Private conversation containers use `Add` wrappers.

Unrecognized activity types are ignored with `202 Accepted`; this response
does not indicate support for the activity.

### Authentication

Inbox delivery requires HTTP signatures. Plamenu uses RFC 9421 for peers whose
support is known and falls back to draft-cavage on rejection. Actor and object
fetches are signed.

With `authorized_fetch` enabled, object and collection GET requests require
signatures. Unsigned actor fetches return either the full profile or a document
containing only identity and key information, controlled by
`authorized_fetch_unsigned_profile`. Discovery and the instance actor remain
public. Restricted objects require authorization for the requesting actor.

### Webxdc

Webxdc bundles are publicly retrievable and checked against their
`digestMultibase`. The accepted media types are `application/webxdc+zip` and
legacy `application/x-webxdc`. Ephemeral packets expire within five seconds
and are not stored or replayed. The wire format is defined in the
[Webxdc FEP](docs/fep/fep-752d.md).

## Receiving limits

| Input | Limit | If exceeded |
| --- | --- | --- |
| Remote poll options | First 100 entries | Later entries are ignored. |
| Remote media attachments | First 100 non-link entries | Later entries are ignored. |
| Unicode reaction content | 16 Unicode scalar values | Reaction is ignored. Custom emoji shortcodes use separate validation. |
| Distinct object IDs in a `Flag` | 50 | Additional IDs are ignored. |

## Additional documentation

- [Protocol details and configuration](docs/federation/protocol.md)
- [Interoperability tests and live-peer coverage](docs/INTEROPERABILITY.md)
- [Signing-key storage, rotation, and recovery](docs/FEDERATION_KEY_OPERATIONS.md)
