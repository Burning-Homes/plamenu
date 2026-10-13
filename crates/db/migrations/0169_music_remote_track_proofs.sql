-- Typed publication snapshots never confer ownership on an imported catalogue.
CREATE TABLE music_remote_track_proofs (
    proof_uri TEXT PRIMARY KEY REFERENCES music_remote_resources(uri) ON DELETE CASCADE,
    uri TEXT NOT NULL REFERENCES music_remote_resources(uri) ON DELETE CASCADE,
    controller_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL DEFAULT 'track' CHECK(kind='track'),
    proof_kind TEXT NOT NULL DEFAULT 'audio' CHECK(proof_kind='audio'),
    name TEXT NOT NULL CHECK(length(name)<=500),
    summary TEXT NOT NULL CHECK(octet_length(summary)<=131072),
    details JSONB NOT NULL CHECK(jsonb_typeof(details)='object' AND octet_length(details::text)<=32768),
    parent_uri TEXT CHECK(length(parent_uri)<=2048),
    fetched_at TIMESTAMPTZ NOT NULL,
    changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX music_remote_track_proofs_target ON music_remote_track_proofs(uri,controller_id,fetched_at DESC);
CREATE OR REPLACE FUNCTION retire_account_remote_music() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_saved_remote WHERE account_id=NEW.id;
        DELETE FROM music_remote_track_proofs WHERE controller_id=NEW.id;
        UPDATE music_remote_resources SET unavailable_at=clock_timestamp(),tombstoned_at=clock_timestamp(),changed_at=clock_timestamp(),
            name='',summary='',details='{}'::jsonb,audio_url=NULL,next_page_uri=NULL WHERE controller_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
