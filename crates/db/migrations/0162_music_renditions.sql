-- Cached encodes are private storage references, never bearer media URLs.
CREATE TABLE music_renditions (
    id BIGINT PRIMARY KEY,
    asset_id BIGINT NOT NULL,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source_hash TEXT NOT NULL CHECK(source_hash ~ '^[a-f0-9]{64}$'),
    format TEXT NOT NULL CHECK(format IN ('mp3','ogg','flac','aac','opus')),
    bitrate INTEGER NOT NULL CHECK(bitrate BETWEEN 32 AND 512),
    actual_bitrate INTEGER CHECK(actual_bitrate BETWEEN 1 AND 512),
    requester_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','ready','failed','cancelled')),
    file_name TEXT UNIQUE CHECK(file_name ~ '^musicrend[0-9]+\.(mp3|ogg|flac|m4a|opus)$'),
    size_bytes BIGINT NOT NULL DEFAULT 0 CHECK(size_bytes>=0),
    budget_bytes BIGINT NOT NULL DEFAULT 0 CHECK(budget_bytes>=0),
    lease BIGINT,
    lease_until TIMESTAMPTZ,
    cleanup_job_id BIGINT,
    error TEXT CHECK(length(error)<=512),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY(asset_id,owner_id) REFERENCES music_assets(id,owner_id) ON DELETE CASCADE,
    UNIQUE(asset_id,source_hash,format,bitrate),
    CHECK(state<>'ready' OR (file_name IS NOT NULL AND size_bytes>0 AND actual_bitrate IS NOT NULL AND cleanup_job_id IS NOT NULL)),
    CHECK(state<>'running' OR (lease IS NOT NULL AND lease_until IS NOT NULL AND budget_bytes>0 AND file_name IS NOT NULL))
);
CREATE INDEX music_renditions_work ON music_renditions(state,lease_until,id) WHERE state IN ('queued','running');
CREATE INDEX music_renditions_owner ON music_renditions(owner_id);
CREATE FUNCTION retire_music_rendition() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE surviving_owner BIGINT;
BEGIN
    IF OLD.state='ready' THEN
        SELECT id INTO surviving_owner FROM accounts WHERE id=OLD.owner_id;
        INSERT INTO media_cleanup_jobs(id,file_name) VALUES(OLD.cleanup_job_id,OLD.file_name)
            ON CONFLICT(file_name) DO NOTHING;
        INSERT INTO music_storage_debt(file_name,owner_id,size_bytes)
            VALUES(OLD.file_name,surviving_owner,OLD.size_bytes)
            ON CONFLICT(file_name) DO UPDATE SET size_bytes=GREATEST(music_storage_debt.size_bytes,EXCLUDED.size_bytes);
    END IF;
    RETURN OLD;
END;
$$;
CREATE TRIGGER music_rendition_retirement AFTER DELETE ON music_renditions
    FOR EACH ROW EXECUTE FUNCTION retire_music_rendition();
CREATE FUNCTION retire_account_music_renditions() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_renditions WHERE owner_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER music_rendition_account_retirement AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_renditions();
