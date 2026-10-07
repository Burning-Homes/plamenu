-- Quarantine an observed contradiction without rewriting the wire declaration.
-- A real lifecycle transition increments the revision; duplicates do not.
ALTER TABLE media_attachments
    ADD COLUMN remote_live_reload_blocked_revision BIGINT
    CHECK (remote_live_reload_blocked_revision >= 0);
