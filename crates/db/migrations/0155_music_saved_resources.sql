-- Private references contain identities, never copies of another owner's music.
ALTER TABLE music_collections ADD CONSTRAINT music_collections_id_kind UNIQUE(id,kind);
CREATE TABLE music_saved_resources (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('track','audio','artist','album','library','playlist','radio')),
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    artist_id BIGINT REFERENCES music_artists(id) ON DELETE CASCADE,
    album_id BIGINT REFERENCES music_albums(id) ON DELETE CASCADE,
    collection_id BIGINT,
    radio_id BIGINT REFERENCES music_radios(id) ON DELETE CASCADE,
    target_id BIGINT GENERATED ALWAYS AS
        (COALESCE(asset_id,status_id,artist_id,album_id,collection_id,radio_id)) STORED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK(num_nonnulls(asset_id,status_id,artist_id,album_id,collection_id,radio_id)=1),
    CHECK((kind='track' AND asset_id IS NOT NULL)
       OR (kind='audio' AND status_id IS NOT NULL)
       OR (kind='artist' AND artist_id IS NOT NULL)
       OR (kind='album' AND album_id IS NOT NULL)
       OR (kind IN ('library','playlist') AND collection_id IS NOT NULL)
       OR (kind='radio' AND radio_id IS NOT NULL)),
    FOREIGN KEY(collection_id,kind) REFERENCES music_collections(id,kind) ON DELETE CASCADE,
    UNIQUE(account_id,kind,target_id)
);
CREATE INDEX music_saved_resources_account ON music_saved_resources(account_id,id DESC);
CREATE INDEX music_saved_resources_asset ON music_saved_resources(asset_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_saved_resources_status ON music_saved_resources(status_id) WHERE status_id IS NOT NULL;
CREATE INDEX music_saved_resources_artist ON music_saved_resources(artist_id) WHERE artist_id IS NOT NULL;
CREATE INDEX music_saved_resources_album ON music_saved_resources(album_id) WHERE album_id IS NOT NULL;
CREATE INDEX music_saved_resources_collection ON music_saved_resources(collection_id) WHERE collection_id IS NOT NULL;
CREATE INDEX music_saved_resources_radio ON music_saved_resources(radio_id) WHERE radio_id IS NOT NULL;
CREATE FUNCTION retire_account_saved_music() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_saved_resources WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_saved_music AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_saved_music();
