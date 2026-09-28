-- Retain the occurrence, not a resurrectable URL, when a cached Video is
-- deleted (hard deletion, thread stub, channel/person purge or moderation).
-- Run before FK SET NULL loses the cached identity. Also mark unresolved
-- duplicate occurrences whose canonical remote URI matches the deleted row.
CREATE INDEX idx_video_playlist_entries_status ON video_playlist_entries(status_id)
WHERE status_id IS NOT NULL;
CREATE INDEX idx_video_playlist_entries_video_uri ON video_playlist_entries(video_uri)
WHERE video_uri <> '';

CREATE FUNCTION video_playlist_video_deleted() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' OR NEW.deleted_at IS NOT NULL THEN
        UPDATE video_playlist_entries SET video_uri = '', status_id = NULL
        WHERE status_id = OLD.id OR (OLD.uri IS NOT NULL AND video_uri = OLD.uri);
    END IF;
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER video_playlist_video_deleted
BEFORE DELETE OR UPDATE OF deleted_at ON statuses
FOR EACH ROW EXECUTE FUNCTION video_playlist_video_deleted();
