-- Durable fixed-window rate-limit counters (QC audit #7).
--
-- The security-sensitive admission budgets (login attempts, password resets,
-- sign-ups, OAuth token minting, app registration, paid translation) must not
-- reset on process restart and must be shared when several app instances run
-- against one database. Postgres is already mandatory, so it is the shared
-- backend; the high-volume load-shedding buckets (general API, paging, remote
-- ingress) deliberately stay in-process.
CREATE TABLE rate_limit_windows (
    bucket text NOT NULL,
    identity text NOT NULL,
    period_index bigint NOT NULL,
    count integer NOT NULL,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (bucket, identity)
);

-- The hourly maintenance sweep deletes expired windows by this index.
CREATE INDEX idx_rate_limit_windows_expires_at ON rate_limit_windows (expires_at);
