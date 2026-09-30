-- Retiring a range cache row is not proof that either physical copy was
-- deleted. Capture its known size atomically, including FK cascades, and keep
-- charging the cache class until the ordinary cleanup worker acknowledges it.
ALTER TABLE media_cleanup_jobs ADD COLUMN remote_cache_bytes BIGINT NOT NULL DEFAULT 0
    CHECK (remote_cache_bytes >= 0);

CREATE FUNCTION retain_remote_cache_cleanup() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO media_cleanup_jobs (id, file_name, remote_cache_bytes)
    VALUES (OLD.id, OLD.cache_file, GREATEST(OLD.bytes, 0))
    ON CONFLICT (file_name) DO UPDATE SET
        remote_cache_bytes = GREATEST(media_cleanup_jobs.remote_cache_bytes, EXCLUDED.remote_cache_bytes);
    RETURN OLD;
END;
$$;

CREATE TRIGGER remote_cache_retirement
    AFTER DELETE ON media_hls_segments
    FOR EACH ROW EXECUTE FUNCTION retain_remote_cache_cleanup();
