-- A broken publisher connection is recoverable for a bounded interval. A
-- process restart can resume the deadline rather than leaving one-shot
-- broadcasts permanently stuck in an off-air state.
ALTER TABLE native_live_broadcasts
    ADD COLUMN reconnect_deadline TIMESTAMPTZ;
ALTER TABLE native_live_broadcasts
    ADD CONSTRAINT native_live_reconnect_deadline_check
    CHECK ((state = 'interrupted') = (reconnect_deadline IS NOT NULL));
CREATE INDEX native_live_reconnect_due
    ON native_live_broadcasts(reconnect_deadline)
    WHERE reconnect_deadline IS NOT NULL;

ALTER TABLE media_attachments DROP CONSTRAINT media_attachments_live_state_check;
ALTER TABLE media_attachments ADD CONSTRAINT media_attachments_live_state_check
    CHECK (live_state IS NULL OR live_state IN ('waiting', 'live', 'interrupted', 'ended'));
