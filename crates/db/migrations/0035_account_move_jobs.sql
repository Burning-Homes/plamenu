-- Replaying an account migration's local relationships is a background job,
-- not part of the request that learned about the move (`BENCH_AUDIT_PLAN.md`
-- D11).
--
-- `migration::process_move` loops every local follower, blocker and muter of
-- the moving account and federates a `Follow` and an `Undo(Follow)` per
-- follower. It ran inline: inside the inbox POST that delivered the `Move`,
-- and inside the form submission of an outbound migration. Both are bounded by
-- how popular the moving account is, which is not a bound — a remote actor
-- with tens of thousands of local followers would hold the inbox request open
-- until the sender timed out and retried, replaying the whole thing.
--
-- The redirect itself (`accounts.moved_to_uri`) stays synchronous: it is one
-- UPDATE, clients must see the new location immediately, and the outbound
-- `Update(Actor)` that follows has to carry it.
--
-- Shaped like the other lease-based queues (`quote_verify_jobs`): a claim
-- pushes `run_at` forward rather than deleting the row, so a worker that
-- crashes mid-replay leaves the job to become due again. `process_move` is
-- idempotent — it re-follows and unfollows per remaining local follower — so a
-- reclaim finds only what the previous attempt did not finish.
CREATE TABLE account_move_jobs (
    id bigint PRIMARY KEY,
    source_account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    target_account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    run_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0
);

-- One pending replay per (source, target). A `Move` is commonly delivered more
-- than once — every follower's server forwards it, and an actor refresh can
-- make a genuine activity look like a replay — and each redelivery must find
-- the queued job rather than add another.
CREATE UNIQUE INDEX idx_account_move_jobs_pair
    ON account_move_jobs (source_account_id, target_account_id);

-- The claim orders by `run_at`; the FK indexes keep an account delete from
-- scanning the queue (the rule stated in 0034).
CREATE INDEX idx_account_move_jobs_run_at ON account_move_jobs (run_at);
CREATE INDEX idx_account_move_jobs_target ON account_move_jobs (target_account_id);
