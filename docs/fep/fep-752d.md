---
slug: "752d"
authors: "lnkr <lnkr@burning.homes>"
status: DRAFT
dateReceived: 2026-08-15
---
# FEP-752d: Federated Webxdc application sessions

## Summary

This FEP maps [Webxdc] application sessions to ActivityPub. Each session has a
`Group` actor that coordinates membership and redistributes participant
activities using `Announce`. Durable updates form a serial log; ephemeral
packets reach connected clients without storage or replay. Invitations are
ordinary `Note` objects with HTTPS links, usable on non-supporting software.

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are
interpreted as specified by [RFC-2119] and [RFC-8174].

## Session actor and bundle

A **session** is one shared application instance, identified by an HTTPS actor
`id`. Its **coordinator** hosts that actor, admits **participants**, sequences
durable updates, and redistributes traffic. A **host** runs the application and
provides the Webxdc API for its user. Two sessions using identical bundle bytes
still have independent membership, state, storage, and channels.

The session actor MUST:

* include both `Group` and `WebxdcSession` in `type`;
* expose `inbox`, `outbox`, and `followers` endpoints;
* set `attributedTo` to its creator or controlling actor;
* set `webxdcProtocol` to `https://w3id.org/fep/752d`;
* identify exactly one immutable bundle as described below;
* advertise `sendUpdateInterval` and `sendUpdateMaxSize`; and
* authenticate its activities using the implementation's ActivityPub
  authentication mechanism.

`followers` MUST contain exactly the actors with active, accepted,
protocol-marked membership. Generic followers MUST NOT receive Webxdc traffic.
The actor SHOULD serve an HTML landing page through content negotiation; it
need not have a WebFinger address.

```json
{
  "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/fep/752d"],
  "id": "https://social.example/webxdc/chess",
  "type": ["Group", "WebxdcSession"],
  "name": "Chess with Alice",
  "attributedTo": "https://social.example/users/alice",
  "inbox": "https://social.example/webxdc/chess/inbox",
  "outbox": "https://social.example/webxdc/chess/outbox",
  "followers": "https://social.example/webxdc/chess/followers",
  "webxdcProtocol": "https://w3id.org/fep/752d",
  "attachment": {
    "id": "https://social.example/webxdc/chess/bundle.xdc",
    "type": "Document",
    "mediaType": "application/webxdc+zip",
    "url": "https://social.example/webxdc/chess/bundle.xdc",
    "digestMultibase": "uEiA-nuhrWADMWAE3jnLwXnYB7ytEgnGqTS9dzxeyQeO04A"
  },
  "sendUpdateInterval": 10000,
  "sendUpdateMaxSize": 128000,
  "published": "2026-08-15T12:00:00Z"
}
```

The bundle attachment MUST be a `Document` with HTTPS `id` and `url` and a
`digestMultibase`: a Multibase-encoded SHA-256 Multihash of the exact `.xdc`
archive bytes [Data-Integrity]. Publishers SHOULD use `application/webxdc+zip`;
consumers MUST also accept the deployed `application/x-webxdc` type. A filename
suffix or arbitrary ZIP attachment alone does not identify an executable
session. Hosts MUST verify the digest before extraction or execution.

The bundle identity, media type, digest, and bytes MUST remain unchanged for the
session's lifetime. Its retrieval URL MAY change if those properties remain the
same. A new application version requires a new session actor.

Public bundle retrieval is the interoperable baseline. A private bundle MUST
require authenticated membership, rather than possession of its URL. Its
retrieval requires a mutually supported actor-authenticated HTTPS mechanism,
which this FEP does not define. A coordinator MUST reject membership if it
cannot supply the bundle to that host. Accepted hosts MUST be able to obtain the
bundle for joining and resuming while the session remains open.

## Invitations and discovery

An invitation MUST be an ordinary `Create` of a `Note` containing a
human-readable HTTPS anchor to the session landing page. It SHOULD also set
`audience` to the session actor and attach a `Link` with that URL,
`mediaType: "text/html"`, and `rel: "https://w3id.org/fep/752d/open"`. The
explicit anchor is required because generic clients may ignore attachments or
extension fields.

```json
{
  "type": "Note",
  "content": "<p>Play <a href=\"https://social.example/webxdc/chess\">chess with Alice</a>.</p>",
  "audience": "https://social.example/webxdc/chess",
  "attachment": {
    "type": "Link",
    "href": "https://social.example/webxdc/chess",
    "mediaType": "text/html",
    "rel": "https://w3id.org/fep/752d/open"
  }
}
```

Supporting hosts resolve and validate the actor before offering a trusted
launch control. Discovery, previewing, or reading an invitation MUST NOT execute
the bundle. Invitation visibility neither grants membership nor encrypts
session content. Invitations are independent posts: copying, editing, or
deleting one does not copy, alter, or end the session.

The landing page SHOULD show the application, coordinator, session state, and
how to join. It MAY offer a coordinator-operated guest runtime or a remote
interaction flow. Visiting the link does not authenticate a remote account;
guest identities MUST be described as guests, and scoped to the session. Guest
management and cross-server browser authentication are outside this FEP.

## Membership and session lifetime

Session lifetime is independent of an application's process or browser window.
An open session persists when nobody is running the app. Closing a window,
disconnecting, or leaving the ephemeral channel does not end membership or the
session. Application-local storage is disposable cache; the durable log is the
shared state used to join or resume.

### Join and resume

A host joins by sending an authenticated `Follow` to the session actor, with
`object`, `context`, `audience`, and `to` identifying that actor and
`webxdcProtocol: "https://w3id.org/fep/752d"`. Generic follows MUST be rejected.
The coordinator checks session state, actor/domain blocks, admission policy,
and resource limits, then responds with `Accept` or `Reject` referencing that
`Follow` or its identifier. HTTP success alone is not membership acceptance.

```json
{
  "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/fep/752d"],
  "id": "https://remote.example/users/bob/follows/chess-1",
  "type": "Follow",
  "actor": "https://remote.example/users/bob",
  "object": "https://social.example/webxdc/chess",
  "to": "https://social.example/webxdc/chess",
  "context": "https://social.example/webxdc/chess",
  "audience": "https://social.example/webxdc/chess",
  "webxdcProtocol": "https://w3id.org/fep/752d"
}
```

An `Accept` MUST be authenticated as the session actor, addressed to the
participant, and include `audience`, `webxdcProtocol`, and `webxdcMaxSerial`.
The latter is the greatest durable serial assigned when acceptance commits,
or zero for an empty log. The host MUST match it to its pending `Follow`.

```json
{
  "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/fep/752d"],
  "id": "https://social.example/webxdc/chess/activities/accept-bob",
  "type": "Accept",
  "actor": "https://social.example/webxdc/chess",
  "object": "https://remote.example/users/bob/follows/chess-1",
  "to": "https://remote.example/users/bob",
  "audience": "https://social.example/webxdc/chess",
  "webxdcProtocol": "https://w3id.org/fep/752d",
  "webxdcMaxSerial": 6
}
```

Acceptance and the replay boundary MUST be atomic: for boundary `B`, serials
`1..B` are replayed and every subsequent update includes the participant in live
fan-out. The coordinator MUST retain the complete durable log while allowing
new joins; inability to supply it requires rejecting new membership.

Hosts MUST NOT submit updates or packets before acceptance. Replay and live
announcements may arrive before or after `Accept`, in any order. Hosts MUST NOT
present the app as synchronized until all serials `1..B` are available.
Ephemeral packets arriving before acceptance or channel connection MUST be
discarded, never held for later launch.

Reopening the app under an active membership resumes the same session,
pseudonymous identity, and durable history. It does not require a fresh
`Follow`. If cached history was discarded or synchronization cannot complete,
the host MAY leave and join with a new `Follow` to obtain a new replay boundary.
An access-controlled `OrderedCollection` of durable announcements MAY optimize
retrieval, but does not replace required inbox replay without a separate
agreement.

### Leave and removal

A participant leaves with an authenticated `Undo` of its accepted `Follow`.
The coordinator MAY remove a participant with an authenticated `Remove`, whose
`object` is the participant, `target` is the session's `followers` collection,
and `audience` is the session. Removal MUST stop authorization for new traffic,
future delivery, private retrieval, and connected ephemeral channels. A host
MUST disable local participation as soon as it leaves or learns of removal.
A new accepted `Follow` is required to rejoin; stale responses to an earlier
membership MUST NOT reactivate it.

Leaving does not retract the participant's accepted durable updates, which
remain necessary to reconstruct shared state. Previously delivered data cannot
be recalled. A host MAY discard its cached session when no local membership
needs it. A coordinator MAY require the controlling user to close or delete
the session instead of leaving it unmanaged; ownership transfer is not defined.

### Close

An open session has no implicit expiry. A coordinator MAY close it under its
published retention or administration policy, including inactivity limits.
Closure MUST be represented by setting `endTime` on the session actor and
sending an authenticated `Update` to current participants. Following
[ActivityPub], `object` MUST contain the complete updated actor, including its
immutable bundle metadata, `endTime`, and `updated`; it is not a partial patch.

Closure is terminal for that actor. The coordinator MUST reject new joins,
durable submissions, and ephemeral packets. Hosts MUST stop sending and close
ephemeral connections when they learn of closure. Delayed durable announcements
already accepted before closure MAY still complete retained history. Read-only
access to the landing page, bundle, or history MAY continue under local policy.
Restarting play requires a new session actor; clearing `endTime` MUST NOT reopen
an existing session.

### Delete

A coordinator MAY delete an open or closed session with an authenticated
`Delete` whose `object` identifies the session actor, optionally as a
`Tombstone` with `formerType: ["Group", "WebxdcSession"]` and `deleted`.
It MUST deliver this activity to the participant snapshot taken before purge.

Deletion terminates participation and removes the bundle, extracted files,
durable log, memberships, and guest credentials held for that session.
Receiving hosts MUST stop its runtimes and channels and purge their cached
session data. A minimal tombstone MUST prevent delayed activities from
recreating the deleted session; an HTTP representation SHOULD return `410 Gone`.
The tombstone need retain only the session identifier and deletion time.
The coordinator MAY retain signing material and delivery metadata needed to
finish sending `Delete`.

Deleting a session does not delete its invitation posts, other sessions using
the same bundle, or copies exported by users. Servers cannot guarantee erasure
from uncooperative recipients or disconnected devices.

## Durable updates

### Submission and validation

A host maps `sendUpdate(update, descr)` to `Create` of a `WebxdcUpdate`, addressed
to the session actor. The deprecated description argument is not federated.
The activity and object MUST have distinct, globally unique identifiers, the
participant as `actor`/`attributedTo`, and the session as `context` and
`audience`. `webxdcUpdate` contains the complete Webxdc update as a JSON literal
(`@type: @json`), including `payload` and any optional or unknown API members.
The payload may itself be any JSON value, including `null` [Webxdc-sendUpdate].

```json
{
  "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/fep/752d"],
  "id": "https://remote.example/users/bob/activities/move-1",
  "type": "Create",
  "actor": "https://remote.example/users/bob",
  "to": "https://social.example/webxdc/chess",
  "context": "https://social.example/webxdc/chess",
  "audience": "https://social.example/webxdc/chess",
  "object": {
    "id": "https://remote.example/users/bob/updates/move-1",
    "type": "WebxdcUpdate",
    "attributedTo": "https://remote.example/users/bob",
    "context": "https://social.example/webxdc/chess",
    "audience": "https://social.example/webxdc/chess",
    "webxdcUpdate": {"payload": {"move": "e2e4"}, "summary": "Black to move"}
  }
}
```

`sendUpdateInterval` is a non-negative integer in milliseconds;
`sendUpdateMaxSize` is a positive byte count. Hosts MUST expose these values to
the app and enforce them before federation. Hosts SHOULD queue early calls in
order. The size limit measures the UTF-8 compact JSON serialization of the
complete update, excluding the ActivityPub envelope. Coordinators MUST allow
sufficient envelope overhead and serialization margin to honor their advertised
limit; member ordering and insignificant whitespace must not invalidate an
otherwise conforming update. Separate abuse and storage quotas are permitted.

The coordinator MUST authenticate the submitting actor, check active membership
and session state, validate both envelopes and the Webxdc update, and enforce
size, rate, and moderation limits. `href` MUST be an in-app relative reference.
Human-readable fields MUST be treated as untrusted text. Invalid submissions
MUST NOT be sequenced or redistributed; the coordinator MAY send `Reject`
referencing the `Create`.

### Sequencing, delivery, and replay

For each accepted update the coordinator atomically assigns the next contiguous
positive `webxdcSerial`, starting at 1, and stores the unchanged `Create`. Serials
MUST NOT exceed JavaScript's maximum safe integer, `9007199254740991`. It wraps
the `Create` in an `Announce` with the session actor as `actor`, the session as
`audience`, its `followers` collection as `to`, and the assigned serial.

The coordinator MUST resolve recipients and deliver the announcement to every
active participant, including the sender, using ordinary durable ActivityPub
delivery. Merely addressing a collection is insufficient. The sender's echoed
announcement, rather than HTTP acceptance of its submission, establishes the
coordinator's acceptance.

Hosts MUST apply durable updates only inside authenticated announcements from
the expected coordinator. Direct participant-to-participant updates are not
authoritative. The coordinator's wrapper vouches for the embedded activity; it
does not prove end-to-end participant authorship.

The coordinator MUST map both accepted input identifiers to their serial and
announcement. Identical redelivery MUST NOT allocate another serial; identifier
reuse with different content or a different counterpart identifier MUST be
rejected. Hosts MUST deduplicate `(session, serial)` and treat conflicting
contents at one serial as an integrity failure. Accepted updates are append-only:
`Update`, `Delete`, or `Undo` MUST NOT mutate individual entries. A coordinator
unable to retain the required history SHOULD close the session.

`setUpdateListener(callback, serial)` delivers locally known updates after
`serial` in increasing serial order, then resolves its promise and continues
with new updates [Webxdc-setUpdateListener]. The callback receives the preserved
update with `serial` from the coordinator and `max_serial` equal to the highest
locally known serial. Hosts MUST tolerate reordered and delayed federation,
maintain the acceptance-boundary completeness check, and SHOULD deliver a
contiguous prefix when possible. Applications must still handle concurrent
changes; the coordinator's order is not distributed consensus.

## Ephemeral channel

### Transport and semantics

This FEP treats Webxdc's “realtime” API as an **ephemeral** channel: an
ActivityPub-compatible baseline, without a low-latency guarantee. It is expected
to underperform in latency-sensitive applications such as competitive reaction
games. Lower-latency approaches, including direct client connections to the
coordinator or overlay networks, require negotiation of other transport profiles;
they do not use ActivityPub as packet transport and are outside this FEP.

Coordinators implementing this FEP MUST support the ephemeral packet envelope.
Hosts MAY omit the experimental `joinRealtimeChannel()` API; when exposed over
this baseline, it MUST follow [Webxdc-Realtime]. Membership authorizes the
channel; opening or leaving it is local connection state and requires no new
federated membership activity.

A packet follows the same participant → coordinator → participant-host path as
a durable update, using `Create` and `Announce`, but MUST NOT receive a serial.
Implementations MUST NOT persist packet payloads or envelopes to databases,
activity archives, inbox/outbox collections, retry jobs, or application logs.
They MUST NOT replay packets or retry failed delivery. Bounded, short-lived
memory buffers and duplicate-ID caches are permitted. Implementations SHOULD
cache routing and authorization metadata to avoid per-packet database work;
membership, signing, and moderation checks still apply.

Only clients with a currently joined channel may receive packets. A receiving
host MUST discard packets if there is no eligible connected client. It MUST NOT
retain them for a later connection, listener registration, acceptance, or app
launch. Slow consumers and overloaded hosts MAY drop packets. Packet delivery
has no acknowledgment, ordering, completeness, or fairness guarantee. The
originating participant MUST NOT receive its own packet as an echo.

### Envelope

A participant sends a `Create` with a `WebxdcEphemeral` object. The activity MUST
have a globally unique `id`, the authenticated participant as `actor`, the
session as `to`, `context`, and `audience`, and the protocol marker. Its object
MUST have a globally unique `id`, the same participant as `attributedTo`, the
session as `context`, and `webxdcData` containing standard, padded RFC 4648
Base64 of the binary packet (not a data URL). The decoded size MUST NOT exceed
128000 bytes; an empty packet is valid.

`published` and `endTime` on the activity are required RFC 3339 timestamps.
`endTime` MUST be later than `published` and no more than five seconds after it.
Each hop MUST discard expired packets and bound any in-memory wait or network
attempt by the remaining lifetime. Receivers MUST reject publication times
more than one second in the future; this small clock tolerance is not a latency
promise. Implementations need reasonably synchronized clocks.

```json
{
  "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/fep/752d"],
  "id": "https://remote.example/users/bob/ephemeral/1",
  "type": "Create",
  "actor": "https://remote.example/users/bob",
  "to": "https://social.example/webxdc/chess",
  "context": "https://social.example/webxdc/chess",
  "audience": "https://social.example/webxdc/chess",
  "webxdcProtocol": "https://w3id.org/fep/752d",
  "published": "2026-08-15T12:05:00Z",
  "endTime": "2026-08-15T12:05:05Z",
  "object": {
    "id": "https://remote.example/users/bob/ephemeral/1#packet",
    "type": "WebxdcEphemeral",
    "attributedTo": "https://remote.example/users/bob",
    "context": "https://social.example/webxdc/chess",
    "webxdcData": "AAEC/w=="
  }
}
```

After validating current membership, session state, limits, and envelope, the
coordinator wraps the unchanged `Create` in an `Announce`. The wrapper MUST have
its own unique `id`, the session actor as `actor`, the protocol marker, the
session as `audience`, and `to` equal to its `followers` collection. It MUST copy
`published` and `endTime` unchanged and MUST NOT include `webxdcSerial`.
It attempts direct authenticated inbox delivery to participant hosts; hosts
without connected clients discard it. Multiple recipients sharing one inbox
SHOULD receive a single delivery. No federated presence protocol is required.

A remote host MUST authenticate the outer activity as its expected coordinator
and validate the embedded packet and unchanged deadline. It MUST NOT apply a
direct participant packet or dereference a missing packet body. HTTP delivery
MUST be signed by the activity's actor; general relay/refetch fallback does not
apply to these non-retrievable activities. Unknown keys require ordinary actor
refresh outside the packet path; losing such packets is allowed.

Duplicate suppression SHOULD use a bounded in-memory `(session, Create.id)`
cache lasting at least until expiry. Suppression may be lost on restart; there
is no exactly-once guarantee. Neither duplicate detection nor rejection should
produce durable per-packet records. Oversized frames, malformed Base64,
cross-session addressing, unauthorized senders, and expired packets MUST be
dropped or rejected before redistribution.

### Host API

`joinRealtimeChannel()` synchronously returns a channel with `setListener`,
`send`, and `leave`. A second join before leaving MUST throw. `setListener`
replaces the callback, which receives a `Uint8Array`; `send` accepts a
`Uint8Array` up to 128000 bytes. Sending while disconnected may silently lose
the packet. Hosts MUST NOT buffer those sends for reconnection. `leave`
invalidates that channel object; rejoining creates a fresh connection without
old packets. Closing the runtime releases the channel without undoing session
membership.

A host MUST enforce the same limits at the trusted bridge and transport
boundary, independently of checks in app-visible JavaScript. It MUST stop
traffic and invalidate connections on leave, removal, closure, or deletion.
Hosts SHOULD apply bounded connection, packet-rate, byte-rate, and concurrent
federation limits. Ephemeral overload MUST NOT enqueue work on the durable
update path. Apps should put recoverable shared state in durable updates.

## Runtime, privacy, and security

Hosts MUST implement the relevant [Webxdc] container and host API requirements.
`selfAddr` MUST be a stable session-scoped pseudonym for the participant across
devices and restarts, and SHOULD be unlinkable across sessions. Actor IDs,
email addresses, signing keys, and credentials MUST NOT be exposed as
`selfAddr`. A persisted random mapping or stable keyed derivation is suitable.
`selfName` is a display name visible to the app; hosts should disclose this.

Bundles are untrusted executable content. Hosts MUST require explicit launch,
isolate storage and execution by session, deny app-initiated Internet access,
and keep account credentials outside the runtime. Browser hosts SHOULD use a
dedicated uncredentialed origin per session and an authenticated `MessageChannel`
to the trusted parent. CSP is defense in depth, not a complete network sandbox.
A runtime that cannot prevent app-initiated network access does not meet
Webxdc's isolation requirement and MUST disclose that limitation.

Server-side fetches MUST enforce HTTPS, SSRF and redirect policy, digest
verification, and time/size limits. Archive extraction MUST bound compressed and
expanded sizes, compression ratio, file count, nesting, and path lengths, and
reject traversal, absolute paths, symlinks, special files, and duplicate or
ambiguously normalized paths. Bundle integrity establishes byte identity, not
application safety or provenance.

Hosts MUST validate bridge origin/channel, session, method, type, and limits.
App-supplied text MUST be rendered as text; `href` must stay inside the package.
Notification text and rates require validation. `sendToChat`, exports, and
external URL opening MUST pass through explicit trusted user review before
posting or leaving the sandbox. No automatic ActivityPub mapping for
`sendToChat` or file import is defined here.

The coordinator controls membership, order, availability, and retention. It can
read or censor traffic and knows participants' actor identities. HTTPS and
restricted addressing do not provide end-to-end encryption. A private
participant collection SHOULD require authorization. Guests, invite tokens,
and bundle access MUST NOT treat a copied invitation URL as account identity.

Implementations MUST apply their actor/domain moderation policies to both
channels and invalidate cached authorization when membership or session state
changes. Packet bodies SHOULD be excluded from reverse-proxy and diagnostic
logging as well as application storage. Multi-process hosts need an ephemeral
fan-out mechanism or routing that reaches their connected clients; a persistent
broker is not a conforming packet store.

Implementations SHOULD ship the protected JSON-LD context and MUST NOT allow
remote contexts to redefine these terms. Plain-JSON implementations MUST
recognize the exact compact names and preserve application JSON literally.
Unknown properties do not authorize another transport.

## Vocabulary and context

The namespace is `https://w3id.org/fep/752d/` [FEP-888d]. The context below is
also supplied as `context.jsonld` for publication at `https://w3id.org/fep/752d`.

| Term | Meaning |
| --- | --- |
| `WebxdcSession` | Additional type of the session `Group` actor |
| `WebxdcUpdate` | Durable Webxdc update object |
| `WebxdcEphemeral` | Non-persistent binary packet object |
| `webxdcProtocol` | Protocol IRI on session and control activities |
| `webxdcUpdate` | Complete Webxdc update as a JSON literal |
| `webxdcData` | Standard Base64 packet data |
| `webxdcSerial` | Positive coordinator-assigned durable serial |
| `webxdcMaxSerial` | Non-negative acceptance replay boundary |
| `sendUpdateInterval` | Minimum durable update interval, milliseconds |
| `sendUpdateMaxSize` | Maximum serialized durable update bytes |

```json
{
  "@context": {
    "@version": 1.1,
    "@protected": true,
    "WebxdcSession": "https://w3id.org/fep/752d/WebxdcSession",
    "WebxdcUpdate": "https://w3id.org/fep/752d/WebxdcUpdate",
    "WebxdcEphemeral": "https://w3id.org/fep/752d/WebxdcEphemeral",
    "webxdcProtocol": {"@id": "https://w3id.org/fep/752d/webxdcProtocol", "@type": "@id"},
    "webxdcUpdate": {"@id": "https://w3id.org/fep/752d/webxdcUpdate", "@type": "@json"},
    "webxdcData": "https://w3id.org/fep/752d/webxdcData",
    "webxdcSerial": {"@id": "https://w3id.org/fep/752d/webxdcSerial", "@type": "http://www.w3.org/2001/XMLSchema#positiveInteger"},
    "webxdcMaxSerial": {"@id": "https://w3id.org/fep/752d/webxdcMaxSerial", "@type": "http://www.w3.org/2001/XMLSchema#nonNegativeInteger"},
    "sendUpdateInterval": {"@id": "https://w3id.org/fep/752d/sendUpdateInterval", "@type": "http://www.w3.org/2001/XMLSchema#nonNegativeInteger"},
    "sendUpdateMaxSize": {"@id": "https://w3id.org/fep/752d/sendUpdateMaxSize", "@type": "http://www.w3.org/2001/XMLSchema#positiveInteger"},
    "digestMultibase": "https://w3id.org/security#digestMultibase"
  }
}
```

## Scope and implementations

An invitation-only publisher need implement only the ordinary linked `Note`.
A session coordinator implements membership, lifecycle, durable replay, and
ephemeral redistribution. A host implements membership, durable consumption,
and isolated execution; its experimental ephemeral API remains optional.
Client REST APIs, guest identity formats, user-interface design, alternative
transport profiles, end-to-end encryption, snapshots/compaction, application
signatures, and coordinator migration are outside this FEP.

Plamenu implements this draft experimentally. Its browser compatibility runtime
isolates account credentials and session origins but does not claim complete
Webxdc network isolation. Independent interoperability has not been established.

## References

* [ActivityPub] [ActivityPub](https://www.w3.org/TR/activitypub/), W3C, 2018.
* [Webxdc] [Webxdc specification](https://webxdc.org/docs/spec/).
* [Webxdc-sendUpdate] [sendUpdate](https://webxdc.org/docs/spec/sendUpdate.html).
* [Webxdc-setUpdateListener] [setUpdateListener](https://webxdc.org/docs/spec/setUpdateListener.html).
* [Webxdc-Realtime] [joinRealtimeChannel](https://webxdc.org/docs/spec/joinRealtimeChannel.html).
* [Data-Integrity] [Resource integrity](https://www.w3.org/TR/vc-data-integrity/#resource-integrity), W3C.
* [RFC-2119] [Key words for use in RFCs](https://www.rfc-editor.org/rfc/rfc2119).
* [RFC-8174] [Ambiguity of uppercase vs lowercase in RFC 2119](https://www.rfc-editor.org/rfc/rfc8174).
* [RFC-4648] [Base encodings](https://www.rfc-editor.org/rfc/rfc4648).
* [FEP-888d] [Using w3id.org/fep for namespaces](https://codeberg.org/fediverse/fep/src/branch/main/fep/888d/fep-888d.md).

## Copyright

CC0 1.0 Universal (CC0 1.0) Public Domain Dedication

To the extent possible under law, the authors of this Fediverse Enhancement
Proposal have waived all copyright and related or neighboring rights to this work.
