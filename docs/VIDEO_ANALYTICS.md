# Private creator delivery analytics

Open **Videos → Manage video channels → your channel → Delivery analytics**.
The ordinary HTML page and keyboard-operated forms work without JavaScript.
Collection is **off for each channel until its current owner opts in**.
Owners and accepted editors can read totals; publishers and former owners
have no implicit access. Only the current owner can change or erase collection.
Channel transfer/revocation and account/channel moderation apply immediately.

## What the numbers mean

The daily UTC aggregates describe successful native media responses which
actually emit body data. They contain a video, date, delivery lane, response
count and emitted body bytes. They never contain viewer accounts, IP addresses,
cookies, device IDs, user agents, referrers, individual events or session logs.
This does not change the separate opt-in private watch-history feature.

These are **not views, unique viewers, watch time, confirmed wire delivery or
P2P traffic estimates**. Range retries, bots and internal origin consumers can
contribute. Clients serving bytes from their own cache or another peer do not.
File/range delivery includes native MP4 and native HLS file ranges. HLS segment
and progressive live/rewind responses have separate lanes. Remote publications,
private drafts, metadata, captions, posters, HEAD, failed requests and bodies
which are never polled do not count. A long live response's subsequent bytes
belong to their current UTC day; its response count belongs to its first byte's
day. A dropped response can include bytes already handed to the HTTP server,
not a claim the client received or decoded them.

Totals are best effort, flushed approximately every 30 seconds and during
graceful shutdown. Crashes, overload and database failures can undercount.
This is deliberately not a durable event queue or a billing ledger.
The page/API shows up to 1,000 most recent video/day/lane rows, with the video's
title and link. The limit is not a claim that older retained rows do not exist.

## Privacy and retention

Choose **1–90 UTC days** (default 30). Reducing retention removes expired totals
in the same settings transaction. Disabling collection deletes the channel's
retained totals. **Erase delivery totals** requires confirmation, preserves
the current collection choice, and ignores unsaved form choices. Erasure and
settings changes fence already-open counters for that channel, without stopping
playback or invalidating other channels' counters. Persisted policy generations
also refuse stale batches from a different collector. Deleting a publication
immediately hides its totals and background expiry removes them; physical media
deletion additionally cascades the aggregate rows.

Expiry works in bounded batches of 4,096 rows. Read APIs filter retention and
deleted publications immediately even while a large expired backlog drains.
Erasure cannot remove copies from old operator backups; those remain subject
to the operator's backup retention/access policy.

## Client API

Use an ordinary OAuth bearer token, not an ambient browser cookie:

- `GET /api/v1/video_channels/{channel_id}/analytics`: `read:media`, current
  owner/editor; returns policy, operator availability, measurement label,
  at most 1,000 daily rows and `possibly_truncated`.
- `PUT` on the same endpoint: `write:media`, current owner; JSON
  `{"enabled":true,"retention_days":30}`.
- `DELETE` on the same endpoint: `write:media`, current owner; erase totals
  without changing collection or retention.

IDs and ISO dates are strings. Responses and refusals are `private, no-store`.
The HTML mutation additionally requires the existing session-derived CSRF
token, and HTML pages vary on Cookie. No analytics are exposed in ActivityPub.

## Operator controls and cost

Set the root-level TOML option **`video_analytics = false`** to disable collection
server-wide. This takes effect on restart, prevents new collection, and removes
retained totals through bounded maintenance. It does not require any additional
service, browser script, plugin or installation step. The setting does not
silently re-enable or rewrite an owner's per-channel choice.

There are no additional media-request SQL reads when no channel has opted in.
Once collection is active, successful candidate deliveries use one indexed
native-channel eligibility lookup, with a 100 ms deadline; failure skips
accounting and does not fail playback. Body accounting preserves streaming,
backpressure and range headers and never reads ahead. Memory admits at most
4,096 pending buckets and 4,096 live policy permits, plus one bounded flush
snapshot. A batch uses array-bound SQL, not a write per viewer/chunk. Statement
and lock deadlines limit background persistence. A database failure drops that
batch without uncertain-commit retry double counting; supervisor health and
worker warnings remain available through ordinary server monitoring.
