# Background queues

Plamenu uses PostgreSQL-backed queues. Workers claim due rows with `FOR UPDATE SKIP LOCKED`.

## Delivery guarantees

Most queues are at-least-once. Claiming a job moves its due time forward but
does not delete it. Completion deletes the row; a crash lets the lease expire
and the job becomes eligible again. Retryable failures reschedule with backoff,
and per-queue attempt limits prevent poison jobs from looping forever. Handlers
on leased queues must therefore be idempotent or safely repeatable.

Account archives and bulk imports use `scheduled`/`in_progress` state plus a
`claimed_at` lease. Startup and periodic sweeps return stale claims to
`scheduled`.

| Queue | Semantics | Recovery |
| --- | --- | --- |
| deliveries, mail, webhooks, Web Push | at-least-once | lease expiry and bounded retry |
| media processing/account media | at-least-once | lease expiry; remote work backs off |
| quote verification | at-least-once | bounded retry, then remains pending |
| reply fetch, link verification/crawl | leased single-attempt work | lease expiry bounds crash loops |
| media cleanup | at-least-once, acknowledged deletion | lease expiry; repeated failures retain revocation and back off for one day |
| account archives, bulk imports | at-least-once state lease | stale-claim sweep |
| scheduled statuses | leased retry; atomic publication | five-minute lease; queue removal commits with the post and delivery jobs |
| poll-expiry side effects | at-most-once | durable close stamp, no notification replay |

Scheduled posts remain listed while claimed or waiting for retry. A failed attempt
becomes eligible again five minutes after its claim; the chosen publication time
is preserved. Claim generations prevent an old worker from publishing after a
retry, cancellation, or reschedule. Queue removal, the post, media attachment, and
outgoing delivery jobs commit together, so retrying cannot create a second post.
Streaming and optional post-commit notifications remain best-effort.

If a scheduled post stays overdue, inspect the `scheduled status publish failed`
log entry. Fix the reported cause or cancel the entry. Rescheduling clears the
retry delay. Failed entries are retained until publication or cancellation.

Poll-expiry notifications remain at-most-once: a crash after the close stamp can
lose those side effects, and replay is not attempted.

## Admission and fairness

Web Push claims rotate across recipient accounts and deliver with bounded
concurrency, so one user or slow endpoint cannot monopolize the queue. Revoking
a token deletes its push subscription and queued jobs.

CSV import upload is limited per account and IP before its body is buffered;
unfinished imports and total pending rows also have server-wide caps. Workers
skip suspended accounts and re-check suspension between write windows.

## Account purge behavior

An account purge keeps its tombstone row, so queue cleanup is explicit:

- retain delivery jobs for the final `Delete(Actor)` or blanked actor update;
- cancel imports, archive builds, account-media work, link verification, and
  scheduled statuses;
- delete Web Push work with the revoked subscriptions;
- enqueue media cleanup for files and stored archive ZIPs.

Media cleanup is inserted transactionally with row deletion. This ensures a
crash cannot leave a database-deleted attachment permanently retrievable from
storage. An enqueued key is refused at the origin before physical removal.
After ten failed claims, the next claim defers another retry burst for one day;
it never discards the revocation key or refunds native storage charges. Repair
the reported permissions/mount problem; the same job completes after the store
acknowledges deletion. Alert on repeated failures and overdue cleanup debt.
`plamenu media reconcile` finds older orphaned files.

Native public playback additionally checks the canonical status, immutable
author/uploader, channel and current channel owner on every origin request.
Deleted/private statuses and suspended/deleted actors refuse MP4, HLS,
captions, storyboard and tracker metadata even after a channel transfer.
Reversible suspension keeps files for later unsuspension. Refusals are
`private, no-store`. Previously fetched bytes and independent browser/CDN/peer
copies cannot be recalled by an origin check; do not treat immutable-byte
caching or distribution controls as DRM.

## Adding a queue

Use a lease whenever accepted work must survive process failure, and make the
operation safe to repeat. If replay is more harmful than rare loss, document
the at-most-once decision at the claim site and add it to the table above.
