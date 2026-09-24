-- The browser's bounded first/last-block fingerprint prevents accidentally
-- resuming a draft with a different local file. NULL denotes the unchanged
-- HTML multipart upload path.
ALTER TABLE video_drafts ADD COLUMN source_fingerprint TEXT
    CHECK (source_fingerprint ~ '^[0-9a-f]{64}$');
