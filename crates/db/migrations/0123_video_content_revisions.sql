-- Social/media identity is stable; content revisions fence playback caches and
-- opt-in positions. Metadata-only edits deliberately do not increment this.
ALTER TABLE media_attachments ADD COLUMN video_content_revision bigint NOT NULL DEFAULT 0
    CHECK (video_content_revision >= 0);
ALTER TABLE video_watch_history ADD COLUMN content_revision bigint NOT NULL DEFAULT 0
    CHECK (content_revision >= 0);

-- Atomic source adoption swaps two existing, independently charged keys. The
-- old source is then removed through the candidate's ordinary cleanup cascade.
ALTER TABLE video_drafts DROP CONSTRAINT video_drafts_source_file_key;
ALTER TABLE video_drafts ADD CONSTRAINT video_drafts_source_file_key UNIQUE (source_file)
    DEFERRABLE INITIALLY IMMEDIATE;
