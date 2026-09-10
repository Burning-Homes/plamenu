-- Durable retention-sweep observability (QC audit #50): one row per sweep
-- (account archives, bulk imports) recording when it last completed
-- successfully and what it did. Logs alone cannot answer "when did retention
-- last actually run?" after rotation; this table can, and the admin dashboard
-- reads it.
CREATE TABLE retention_sweeps (
    name text PRIMARY KEY,
    last_success_at timestamptz NOT NULL,
    last_swept bigint NOT NULL,
    last_retained bigint NOT NULL DEFAULT 0
);
