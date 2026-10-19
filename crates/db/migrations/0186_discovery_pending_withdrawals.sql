-- A verified withdrawal may arrive before the first catalogue write. Keep the
-- already-admitted queue identity as a bounded guard, rather than inventing
-- an actor kind/evidence payload or letting another stale hint recreate work.
ALTER TABLE discovery_jobs ADD COLUMN withdrawn boolean NOT NULL DEFAULT false;
ALTER TABLE discovery_jobs ADD CONSTRAINT discovery_withdrawn_job_unleased
    CHECK (NOT withdrawn OR (lease_token IS NULL AND cursor IS NULL));
