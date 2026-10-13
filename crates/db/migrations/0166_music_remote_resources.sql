-- Canonical remote music is cached separately from locally owned catalogues.
CREATE TABLE music_remote_resources (
    id BIGINT PRIMARY KEY,
    uri TEXT NOT NULL UNIQUE CHECK(length(uri)<=2048),
    proof_uri TEXT NOT NULL REFERENCES music_remote_resources(uri) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    proof_kind TEXT NOT NULL CHECK(proof_kind IN('audio','track','artist','album','library','playlist')),
    controller_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN('audio','track','artist','album','library','playlist')),
    name TEXT NOT NULL CHECK(length(name)<=500),
    summary TEXT NOT NULL CHECK(octet_length(summary)<=131072),
    details JSONB NOT NULL DEFAULT '{}'::jsonb CHECK(jsonb_typeof(details)='object' AND octet_length(details::text)<=32768),
    media_of TEXT CHECK(length(media_of)<=2048),
    parent_uri TEXT CHECK(length(parent_uri)<=2048),
    audio_url TEXT CHECK(length(audio_url)<=2048),
    next_page_uri TEXT CHECK(length(next_page_uri)<=2048),
    fetched_at TIMESTAMPTZ NOT NULL,
    changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    unavailable_at TIMESTAMPTZ,
    tombstoned_at TIMESTAMPTZ
);
CREATE INDEX music_remote_resources_controller ON music_remote_resources(controller_id,id DESC);
CREATE INDEX music_remote_resources_parent ON music_remote_resources(parent_uri,id);
CREATE INDEX music_remote_resources_media_of ON music_remote_resources(media_of,id);
CREATE TABLE music_saved_remote (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    resource_id BIGINT NOT NULL REFERENCES music_remote_resources(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(account_id,resource_id)
);
CREATE INDEX music_saved_remote_account ON music_saved_remote(account_id,id DESC);
CREATE INDEX music_saved_remote_resource ON music_saved_remote(resource_id);
CREATE FUNCTION retire_account_remote_music() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_saved_remote WHERE account_id=NEW.id;
        UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),tombstoned_at=clock_timestamp(),changed_at=clock_timestamp(),
            name='',summary='',details='{}'::jsonb,audio_url=NULL,next_page_uri=NULL WHERE controller_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_remote_music AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_remote_music();
