# Known per-item query fan-outs

This is the current list of confirmed database round-trip loops. It is not an
audit journal: resolved entries and rejected findings belong in version
history. A benchmark query budget is a tripwire against regression, while this
file records debt that the benchmark dataset may not expose.

When fixing an entry, delete it in the same change and record the measured
before/after statement count. A lower benchmark budget needs an explanation too;
dataset changes can lower counts without improving code.

| Entry | Severity | Bound | Why it remains |
| --- | --- | --- | --- |
| `migration-process-move-per-local-blocker` | high | unbounded local blockers | Each carried relationship has distinct federation and moderation effects. |
| `migration-process-move-per-local-follower` | high | unbounded local followers | Each relationship produces distinct follow/unfollow work. |
| `ingest-store-remote-attachments` | medium | 100 per Note | Inbound attachment rows are inserted individually. |
| `inbox-flag-report-per-target` | low | 50 targets | Each local target receives a distinct report; instance reports may also enqueue webhooks. |
| `ingest-thread-ancestor-backfill` | low | 5 ancestors | Each missing ancestor requires guarded network fetch and full ingest. |

## Details

### Move replay

`process_move` in `crates/server/src/migration.rs` loops over local followers and
local accounts blocking the source. Both loops are unbounded and run from the
inbound `Move` request path. Each item performs relationship checks, mutations,
notifications, and/or distinct ActivityPub delivery. No benchmark currently
exercises this path with a SQL query budget.

### Remote attachments

`store_remote_attachments` in `crates/server/src/ingest.rs` calls
`media::create_remote` per attachment. Input is capped at
`MAX_REMOTE_ATTACHMENTS = 100`, so this is bounded but a hostile Note can still
cause 100 serial inserts. The inline-image fallback is deduplicated separately.

### Flag targets

The instance and group `handle_flag` paths in
`crates/server/src/routes/inbox.rs` file one report per deduplicated local
target. `MAX_FLAG_OBJECTS` caps the set at 50. Instance-level reports also run
the configured report webhook path; group-scoped reports do not.

### Thread ancestor backfill

`resolve_thread_parent` in `crates/server/src/ingest.rs` walks and then ingests
up to `MAX_THREAD_ANCESTORS = 5`. Each step includes a guarded remote fetch and
a complete remote-note ingest. This is fixed-bound rather than a traditional
unbounded N+1, but its body is expensive enough to track here.

Single SQL statements containing correlated subqueries are query-plan costs,
not extra round trips, and do not belong in this inventory. Investigate those
with `EXPLAIN` and performance benchmarks instead.
