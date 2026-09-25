-- Durable progress for the bounded, in-process attribution backfill.
-- A NULL retry_at means a canonical object was checked and has no verified
-- channel (or is permanently gone); a timestamp is a lease or retry time.
CREATE TABLE remote_video_channel_checks (
    status_id BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    retry_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0,
    checked_at TIMESTAMPTZ,
    CHECK (attempts >= 0)
);
CREATE INDEX remote_video_channel_checks_retry
    ON remote_video_channel_checks(retry_at, status_id)
    WHERE retry_at IS NOT NULL;
