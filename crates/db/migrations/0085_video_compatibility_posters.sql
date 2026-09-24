-- Preserve efficient native previews while providing an interoperable poster.
-- Immutable derivatives share their source media's lifetime, including uploads
-- abandoned before publication. Reserve a cleanup job id at creation so every
-- cascade can queue file removal atomically without a separate id generator.
CREATE TABLE video_compatibility_posters (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    file_name TEXT NOT NULL UNIQUE,
    width INTEGER NOT NULL CHECK (width > 0 AND width <= 640),
    height INTEGER NOT NULL CHECK (height > 0 AND height <= 640),
    file_size BIGINT NOT NULL CHECK (file_size > 0),
    cleanup_job_id BIGINT NOT NULL UNIQUE
);

CREATE FUNCTION cleanup_video_compatibility_poster() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO media_cleanup_jobs (id, file_name)
    VALUES (OLD.cleanup_job_id, OLD.file_name)
    ON CONFLICT (file_name) DO NOTHING;
    RETURN OLD;
END;
$$;

CREATE TRIGGER video_compatibility_poster_cleanup
AFTER DELETE ON video_compatibility_posters
FOR EACH ROW EXECUTE FUNCTION cleanup_video_compatibility_poster();
