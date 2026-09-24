-- Creator-selected frame for the first generated poster. Existing drafts keep
-- the historical first-frame behavior. This is metadata for the durable job,
-- not permission to regenerate or overwrite a published poster.
ALTER TABLE video_drafts
    ADD COLUMN poster_time_seconds DOUBLE PRECISION NOT NULL DEFAULT 0
        CHECK (poster_time_seconds >= 0 AND poster_time_seconds <= 86400);
