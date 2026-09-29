-- Editing prepares an independent, private source and rendition set. The
-- published film is never placed back into the processing queue.
ALTER TABLE video_drafts
    ADD COLUMN force_reencode boolean NOT NULL DEFAULT false,
    ADD COLUMN clip_start_ms bigint NOT NULL DEFAULT 0,
    ADD COLUMN clip_end_ms bigint,
    ADD CONSTRAINT video_drafts_clip_window CHECK (
        clip_start_ms >= 0 AND
        ((clip_end_ms IS NULL AND clip_start_ms = 0) OR
         (clip_end_ms > clip_start_ms AND clip_end_ms <= 604800000)));

ALTER TABLE video_source_imports DROP CONSTRAINT video_source_imports_source_kind_check;
ALTER TABLE video_source_imports ADD CONSTRAINT video_source_imports_source_kind_check
    CHECK (source_kind IN ('file', 'activitypub', 'native'));

-- Deleting the original must remain possible. A queued edit then fails closed;
-- its independently charged private draft can still be cancelled normally.
CREATE TABLE video_source_edits (
    media_id bigint PRIMARY KEY REFERENCES video_source_imports(media_id) ON DELETE CASCADE,
    target_media_id bigint REFERENCES media_attachments(id) ON DELETE SET NULL,
    target_revision bigint NOT NULL CHECK (target_revision >= 0),
    operation text NOT NULL CHECK (operation IN ('reprocess', 'trim'))
);
