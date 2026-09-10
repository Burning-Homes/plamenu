-- Queue durability (QC audit finding #1). ARCHITECTURE.md promised that
-- "background work uses database-backed queues and leases so a process restart
-- does not lose accepted work", but several queues deleted their row at claim
-- time (at-most-once) or flipped a state column with no lease, so a crash
-- between claim and completion stranded or lost accepted work.
--
-- The fix converts those queues to the lease pattern already used by the
-- delivery queue (`delivery_jobs`): a claim leases the row (pushes `run_at`
-- into the future) instead of deleting it, and the worker deletes the row only
-- once the work has completed. A crash simply lets the lease expire and the row
-- becomes claimable again. This migration adds the columns those queues were
-- missing.

-- Best-effort crawl/verification queues gain an `attempts` counter so a job
-- that repeatedly outlives a *process* crash (reclaimed by lease expiry rather
-- than by a logical retry) is eventually dropped instead of looping forever.
-- Their logical outcome is still single-attempt: a fetch that fails is not
-- retried, matching Mastodon; only lease reclaims bump the counter.
ALTER TABLE reply_fetch_jobs
    ADD COLUMN attempts integer NOT NULL DEFAULT 0;
ALTER TABLE link_verification_jobs
    ADD COLUMN attempts integer NOT NULL DEFAULT 0;
ALTER TABLE link_crawl_jobs
    ADD COLUMN attempts integer NOT NULL DEFAULT 0;

-- The archive and import queues drive work with a `state` text column
-- (`scheduled` -> `in_progress` -> `finished`) rather than `run_at`, and had no
-- lease: an `in_progress` row whose worker crashed stayed `in_progress` forever
-- (never reclaimed) or was eventually GC'd unfinished. `claimed_at` records when
-- a row was leased so a stale-claim sweep can return a crashed `in_progress` row
-- to `scheduled`. NULL while scheduled/finished; set at claim.
ALTER TABLE account_archives
    ADD COLUMN claimed_at timestamp with time zone;
ALTER TABLE bulk_imports
    ADD COLUMN claimed_at timestamp with time zone;
