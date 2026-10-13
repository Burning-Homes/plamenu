-- Revocable capabilities retain exact typed targets; raw secrets are never stored.
ALTER TABLE music_radios ADD CONSTRAINT music_radios_owned_id UNIQUE(id,owner_id);
CREATE TABLE music_share_links (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    artist_id BIGINT REFERENCES music_artists(id) ON DELETE CASCADE,
    album_id BIGINT REFERENCES music_albums(id) ON DELETE CASCADE,
    collection_id BIGINT REFERENCES music_collections(id) ON DELETE CASCADE,
    radio_id BIGINT REFERENCES music_radios(id) ON DELETE CASCADE,
    target_id BIGINT GENERATED ALWAYS AS
        (COALESCE(asset_id,artist_id,album_id,collection_id,radio_id)) STORED,
    token_hash TEXT NOT NULL UNIQUE CHECK (length(token_hash)=43),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    CHECK(num_nonnulls(asset_id,artist_id,album_id,collection_id,radio_id)=1),
    CHECK((kind='track' AND asset_id IS NOT NULL)
       OR (kind='artist' AND artist_id IS NOT NULL)
       OR (kind='album' AND album_id IS NOT NULL)
       OR (kind IN ('library','playlist') AND collection_id IS NOT NULL)
       OR (kind='radio' AND radio_id IS NOT NULL)),
    FOREIGN KEY(collection_id,kind) REFERENCES music_collections(id,kind) ON DELETE CASCADE,
    FOREIGN KEY(asset_id,owner_id) REFERENCES music_assets(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(artist_id,owner_id) REFERENCES music_artists(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(album_id,owner_id) REFERENCES music_albums(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(collection_id,owner_id) REFERENCES music_collections(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(radio_id,owner_id) REFERENCES music_radios(id,owner_id) ON DELETE CASCADE
);
CREATE INDEX music_share_links_owner ON music_share_links(owner_id,id DESC);
-- Grant identities and dates let the owner page every kind of sharing together.
ALTER TABLE music_asset_grants ADD COLUMN id BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_collection_grants ADD COLUMN id BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_radio_grants ADD COLUMN id BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_album_grants ADD COLUMN id BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE;
CREATE FUNCTION retire_account_music_links() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_share_links WHERE owner_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_links AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_links();

-- Each grant owns its notification; revoke, source deletion and account erasure
-- remove that event without retaining private title snapshots.
ALTER TABLE music_asset_grants ADD COLUMN notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL;
ALTER TABLE music_album_grants ADD COLUMN notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL;
ALTER TABLE music_collection_grants ADD COLUMN notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL;
ALTER TABLE music_radio_grants ADD COLUMN notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL;
CREATE FUNCTION retract_music_share_notification() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM notifications WHERE id=OLD.notification_id;
    RETURN OLD;
END;
$$;
CREATE TRIGGER retract_music_asset_share_notification AFTER DELETE ON music_asset_grants FOR EACH ROW EXECUTE FUNCTION retract_music_share_notification();
CREATE TRIGGER retract_music_album_share_notification AFTER DELETE ON music_album_grants FOR EACH ROW EXECUTE FUNCTION retract_music_share_notification();
CREATE TRIGGER retract_music_collection_share_notification AFTER DELETE ON music_collection_grants FOR EACH ROW EXECUTE FUNCTION retract_music_share_notification();
CREATE TRIGGER retract_music_radio_share_notification AFTER DELETE ON music_radio_grants FOR EACH ROW EXECUTE FUNCTION retract_music_share_notification();
