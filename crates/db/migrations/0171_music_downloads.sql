-- Private ZIP requests retain source references and current access decisions.
CREATE TABLE music_downloads (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    label TEXT NOT NULL CHECK(char_length(label) BETWEEN 1 AND 256),
    selection JSONB NOT NULL CHECK(jsonb_typeof(selection)='object'),
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','ready','failed','cancelled')),
    revision BIGINT NOT NULL DEFAULT 1 CHECK(revision>0),
    completed_items INTEGER NOT NULL DEFAULT 0 CHECK(completed_items>=0),
    included INTEGER[] NOT NULL DEFAULT '{}',
    item_errors JSONB NOT NULL DEFAULT '[]'::jsonb CHECK(jsonb_typeof(item_errors)='array'),
    notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL,
    error TEXT CHECK(char_length(error)<=512),
    lease BIGINT,
    lease_until TIMESTAMPTZ,
    file_name TEXT UNIQUE CHECK(file_name ~ '^musicdownload[0-9]+-[0-9]+\.zip$'),
    size_bytes BIGINT NOT NULL DEFAULT 0 CHECK(size_bytes>=0),
    budget_bytes BIGINT NOT NULL DEFAULT 0 CHECK(budget_bytes>=0),
    cleanup_job_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ NOT NULL,
    CHECK((lease IS NULL)=(lease_until IS NULL)),
    CHECK(state<>'ready' OR (file_name IS NOT NULL AND size_bytes>0 AND cleanup_job_id IS NOT NULL))
);
CREATE INDEX music_downloads_work ON music_downloads(id) WHERE state IN ('queued','running');
CREATE INDEX music_downloads_owner ON music_downloads(owner_id,id DESC);
CREATE FUNCTION retire_music_download() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE surviving_owner BIGINT;
BEGIN
    DELETE FROM notifications WHERE id=OLD.notification_id;
    IF OLD.state='ready' THEN
        SELECT id INTO surviving_owner FROM accounts WHERE id=OLD.owner_id;
        INSERT INTO media_cleanup_jobs(id,file_name) VALUES(OLD.cleanup_job_id,OLD.file_name)
            ON CONFLICT(file_name) DO NOTHING;
        INSERT INTO music_storage_debt(file_name,owner_id,size_bytes)
            VALUES(OLD.file_name,surviving_owner,OLD.size_bytes)
            ON CONFLICT(file_name) DO UPDATE SET size_bytes=GREATEST(music_storage_debt.size_bytes,EXCLUDED.size_bytes);
    ELSE
        UPDATE media_cleanup_jobs SET run_at=now() WHERE file_name=OLD.file_name;
    END IF;
    RETURN OLD;
END;
$$;
CREATE TRIGGER music_download_retirement AFTER DELETE ON music_downloads
    FOR EACH ROW EXECUTE FUNCTION retire_music_download();
CREATE FUNCTION retire_account_music_downloads() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_downloads WHERE owner_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER music_download_account_retirement AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_downloads();
