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
| media cleanup | at-least-once | lease expiry and bounded retry |
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
storage. `plamenu media reconcile` finds older orphaned files.

## Adding a queue

Use a lease whenever accepted work must survive process failure, and make the
operation safe to repeat. If replay is more harmful than rare loss, document
the at-most-once decision at the claim site and add it to the table above.
