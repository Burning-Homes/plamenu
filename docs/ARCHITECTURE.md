# Architecture

Plamenu is a Rust workspace with four internal crates:

- `plamenu-ap`: ActivityStreams types, discovery, canonical URLs, signatures,
  integrity proofs, and text normalization.
- `plamenu-db`: PostgreSQL persistence, forward migrations, durable queues, and
  Snowflake-style identifiers.
- `plamenu-federation`: guarded outbound HTTP, content negotiation, and HTTP
  signature verification.
- `plamenu`: axum server, APIs, workers, CLI, and server-rendered interface.

PostgreSQL stores application data. The configured media store holds file bytes;
small in-process caches accelerate settings, authentication, translation, and
rendering but are disposable and invalidated by mutation or short TTLs.
Migrations run before the server accepts traffic.

## Writes and background work

Local post, interaction, relationship, profile, group, collection, RSVP, report,
relay, migration, and key-rotation actions use transactions to commit their
mutations and outgoing delivery jobs together. Remote resolution precedes the
transaction; optional notifications, streams, and webhooks follow commit.
Connection-scoped helpers keep audience and object rendering on that transaction.
Failure-injection coverage lives in `atomic_outbox.rs` and `atomic_mutations.rs`
under `crates/server/tests`. This is not a blanket atomicity guarantee for every
inbound handler or background job; those have their own retry boundaries.

Workers use PostgreSQL-backed queues. Most jobs are leased and therefore
at-least-once: a crash leaves the row to be reclaimed, so handlers must be
idempotent. Scheduled publication removes its claimed entry in the transaction
that creates the post and delivery jobs; claim generations reject stale attempts.
Poll-expiry side effects remain at-most-once. The complete classification is in
[QUEUES.md](QUEUES.md).

Each worker is supervised and restarted after an unexpected exit. Release
builds use `panic = "unwind"` so the supervisor can contain task panics; the
container restart policy handles process-wide failures.

## Deployment boundary

Run one `plamenu serve` process per database. The server enforces this with a
PostgreSQL advisory lock because identifier generation is process-local.
The lock session is monitored during startup, serving, and shutdown. A failed
heartbeat or a five-second heartbeat timeout terminates the process; it never
reconnects and continues without the lock. Detection is not instantaneous.
One CLI command can run alongside the server: it holds a
separate monitored lock and allocates odd IDs, while the server allocates even
IDs. Concurrent CLI commands are refused before initialization. Use matching CLI
and server binaries; mixed-version online administration is not supported.
Each lane remains process-local, so this is not database fencing or a guarantee
across clock rollback followed by process restart. Horizontal multi-server
serving is not supported.

All local ActivityPub identifiers are minted through `crates/ap/src/urls.rs`.
The public domain is therefore permanent after federation begins. Remote input
crosses URL/SSRF, signature, ownership, size, and HTML-sanitization boundaries
before it is persisted or rendered.

See [protocol and extensions](federation/protocol.md) for federation behavior.
