-- Severing a user-level domain block's relationships is a background job, not
-- part of the request that recorded the block (`docs/NPLUS1_DECISIONS.md`
-- D4a).
--
-- `actions::block_domain` looped every follow edge in either direction with
-- the blocked domain and severed each one inline — ~5-6 statements per
-- relationship, inside the API request or web form submission, unbounded by
-- anything but how entangled the user was with that instance. The block row
-- itself stays synchronous: it is one INSERT and clients must see
-- `domain_blocking` immediately. The relationships remain visible for the
-- seconds until the job runs — exactly Mastodon's semantics
-- (`AfterAccountDomainBlockWorker`), which every client already tolerates.
--
-- Shaped like the other lease-based queues (`account_move_jobs`): a claim
-- pushes `run_at` forward rather than deleting the row, so a worker that
-- crashes mid-severance leaves the job to become due again. The severance is
-- set-based and idempotent — a reclaim finds only what the previous attempt
-- did not delete — and a vanished peer never wedges it: deliveries are queued
-- through the ordinary delivery queue, which owns retries and gives up on its
-- own schedule.
CREATE TABLE domain_severance_jobs (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    domain text NOT NULL,
    run_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0
);

-- Re-blocking the same domain while a severance is queued must find the
-- queued job rather than add another.
CREATE UNIQUE INDEX idx_domain_severance_jobs_pair
    ON domain_severance_jobs (account_id, domain);

-- The claim orders by `run_at`; the FK index keeps an account delete from
-- scanning the queue (the rule stated in 0034).
CREATE INDEX idx_domain_severance_jobs_run_at ON domain_severance_jobs (run_at);
