-- Legacy inferred albums keep their current track-derived metadata policy.
ALTER TABLE music_albums
    ADD COLUMN visibility TEXT NOT NULL DEFAULT 'source'
        CHECK (visibility IN ('source','private','local','public')),
    ADD COLUMN share_tracks BOOLEAN NOT NULL DEFAULT false;
CREATE TABLE music_album_grants (
    album_id BIGINT NOT NULL REFERENCES music_albums(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(album_id,account_id)
);
CREATE INDEX music_album_grants_account ON music_album_grants(account_id,album_id);
CREATE INDEX music_assets_album_owner ON music_assets(album_id,owner_id);
-- Even a direct tombstone update must wait for recipient references admitted
-- before it, so the following cleanup cannot miss a still-committing grant.
-- Key-share recipient locks remain compatible with reciprocal owner mutations.
CREATE FUNCTION fence_account_music_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM id FROM accounts WHERE id=NEW.id FOR UPDATE;
    RETURN NEW;
END;
$$;
CREATE TRIGGER fence_account_music_grants BEFORE UPDATE OF deleted_at ON accounts
    FOR EACH ROW WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION fence_account_music_grants();
CREATE FUNCTION retire_account_album_grants() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_asset_grants WHERE account_id=NEW.id;
        DELETE FROM music_collection_grants WHERE account_id=NEW.id;
        DELETE FROM music_album_grants WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_album_grants AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_album_grants();
