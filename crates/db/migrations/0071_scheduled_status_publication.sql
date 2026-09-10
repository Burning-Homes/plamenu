-- Keep scheduled posts until their publication transaction commits. A claim
-- generation fences workers whose lease expired or whose post was rescheduled.
ALTER TABLE scheduled_statuses
    ADD COLUMN publish_after timestamptz,
    ADD COLUMN publish_generation bigint NOT NULL DEFAULT 0;

CREATE INDEX idx_scheduled_statuses_publish_due
    ON scheduled_statuses (GREATEST(scheduled_at, publish_after));
