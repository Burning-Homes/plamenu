# Discovery and remote history

**Administration → Discovery** requires **Manage federation**. It manages a
bounded catalogue for the member Discover pages, separate from normal delivery,
search of already known posts and remote profile history.

## Start or change acquisition

1. Enable discovery and leave **Pause updates** off when you want acquisition.
2. Add a server address, import a source list or explicitly choose bundled
   starters. A starter list is a convenience, not an endorsement or availability
   guarantee. Imported sources must pass verification before being shown.
3. Choose mechanisms for directories, peer exploration, public outboxes,
   indexing, recommendations, video catalogues and on-demand wider search.
   **Peer sharing** is reserved for future work; enabling it does not add it.
4. Set request and retained-entry budgets suitable for your instance. Inspect
   the resulting queue, source states and acquisition cost before increasing them.

Missing discovery/indexing preferences have explicit policy controls. An
explicit refusal must not be treated as permission. Domain restrictions,
source identity, origin validation and current access still apply. A stored
reference is not proof that a source remains authorized or available.

Source-list import/export uses version 1 JSON, up to 100 sources and 64 KiB.
Export the current list as a format example. **Disable bundled sources absent
from this list** affects bundled entries, not a wholesale replacement of all
operator policy. Keep private diagnostics out of public support requests.

## Read the state

| State | Action |
| --- | --- |
| Pending / Needs refresh | Let the worker verify; inspect queue and budgets |
| Ready | Recently verified; content still needs its own access checks |
| Retrying later | Read the recorded error and retry time; fix DNS/TLS/policy if needed |
| Allowance exhausted | Wait for the rolling 24-hour charges to expire, or deliberately adjust limits |
| Paused | Existing catalogue remains; resume when acquisition should continue |
| Disabled | Check global, mechanism and source settings before diagnosing the network |

Requests, charged bytes and per-origin limits include acquisition work; icons
and auxiliary metadata are not free. The console separates costs by content and
operation and shows retained versus active inventory. Do not diagnose an empty
member page solely from the total retained count: freshness, consent, filters,
blocks and seen-state affect eligibility.

Use the matching binary and explicit configuration for CLI diagnostics:

```sh
plamenu --config /etc/plamenu/plamenu.toml discovery status
plamenu --config /etc/plamenu/plamenu.toml discovery sources export
```

See [CLI help](../reference/cli.md) for source import, configuration, pause/resume,
recheck and one-shot acquisition. Mutation commands make real requests and
changes; they are not health probes. Serialize them with other administrative
CLI work.

## Remote profile history

**Administration → History** controls fetching older posts when a signed-in
member opens a remote profile. This is independent of Discover's recommendation
catalogue. It is enabled by default, including bounded resolution of bare item
links. Disable it to stop new acquisition; queued work remains for later resume.

The profile shows queued/fetching, partial, complete, unsupported or backoff.
**Load older posts** requests another bounded page; complete means the observed
collection ended, not that every post ever written was recovered. Historical
imports should not create notifications or outgoing activities.

Retention defaults to 90 days. The console shows cold storage by origin and
provides **Prune one batch now**. Local references such as bookmarks, replies
and reports protect relevant posts. Review counts before shortening retention;
pruned content is recoverable only if the origin still serves it. For detailed
limits and client behavior, see [remote history reference](../REMOTE_HISTORY_OPERATOR_RUNBOOK.md).
