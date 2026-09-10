-- Durable media-file cleanup queue (QC audit finding #39). Deleting a status or
-- account removes the `media_attachments` / `media_hls_segments` rows (usually
-- by ON DELETE CASCADE) that named the stored files, but nothing removed the
-- files themselves — and `GET /media/{file}` serves any stored key without a
-- database lookup, so a deleted attachment's bytes stayed publicly retrievable
-- from the origin forever.
--
-- The fix captures every storage key *before* its row vanishes and enqueues it
-- here, in the same transaction as the deletion, so the physical removal is
-- durable: a crash between "row deleted" and "file deleted" simply leaves a job
-- the worker retries, instead of orphaning the bytes. This is the lease pattern
-- (see docs/QUEUES.md): a claim pushes `run_at` into the future and the worker
-- deletes the row only once the file is gone; `attempts` bounds a job that keeps
-- failing (e.g. a permanently unreadable path) so it is dropped rather than
-- looping forever.
CREATE TABLE media_cleanup_jobs (
    id bigint PRIMARY KEY,
    -- The store key to delete. Unique so enqueuing the same file twice (a delete
    -- racing the reconciliation sweep) collapses to one job.
    file_name text NOT NULL UNIQUE,
    run_at timestamp with time zone NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0,
    created_at timestamp with time zone NOT NULL DEFAULT now()
);

CREATE INDEX media_cleanup_jobs_run_at_idx ON media_cleanup_jobs (run_at);
