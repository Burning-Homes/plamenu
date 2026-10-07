-- Preserve explicit remote lifetime declarations independently of the legacy
-- rendering flag. NULL includes all existing rows: never infer a declaration.
ALTER TABLE media_attachments
    ADD COLUMN remote_live_permanent_declared BOOLEAN,
    ADD COLUMN remote_live_lifecycle_revision BIGINT NOT NULL DEFAULT 0
        CHECK (remote_live_lifecycle_revision >= 0);
