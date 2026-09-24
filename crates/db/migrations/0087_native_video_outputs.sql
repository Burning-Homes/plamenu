-- Reserve immutable attempt outputs before any final storage writes. Keep the
-- reservation through publication, so cascades also clean outputs left by a
-- crashed transaction. A recognizable namespace remains private even after
-- deletion of the journal, until the cleanup worker removes the bytes.
CREATE TABLE video_draft_outputs (
    file_name TEXT PRIMARY KEY
        CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif)$'),
    media_id BIGINT NOT NULL REFERENCES video_drafts(media_id) ON DELETE CASCADE,
    generation BIGINT NOT NULL CHECK (generation > 0),
    retired BOOLEAN NOT NULL DEFAULT false,
    cleanup_job_id BIGINT NOT NULL UNIQUE
);
CREATE INDEX video_draft_outputs_attempt ON video_draft_outputs(media_id, generation);

CREATE FUNCTION cleanup_video_draft_output() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO media_cleanup_jobs (id, file_name)
    VALUES (OLD.cleanup_job_id, OLD.file_name)
    ON CONFLICT (file_name) DO NOTHING;
    RETURN OLD;
END;
$$;
CREATE TRIGGER video_draft_output_cleanup AFTER DELETE ON video_draft_outputs
FOR EACH ROW EXECUTE FUNCTION cleanup_video_draft_output();
