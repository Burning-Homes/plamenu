-- Artwork uses the same non-bearer namespace and current source authorization.
ALTER TABLE music_assets ADD CONSTRAINT music_assets_owned_id UNIQUE(id,owner_id);
ALTER TABLE music_collections ADD CONSTRAINT music_collections_owned_id UNIQUE(id,owner_id);
CREATE TABLE music_artworks (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    asset_id BIGINT,
    artist_id BIGINT,
    album_id BIGINT,
    collection_id BIGINT,
    target_kind TEXT GENERATED ALWAYS AS (CASE WHEN asset_id IS NOT NULL THEN 'assets' WHEN artist_id IS NOT NULL THEN 'artists' WHEN album_id IS NOT NULL THEN 'albums' ELSE 'collections' END) STORED,
    target_id BIGINT GENERATED ALWAYS AS (COALESCE(asset_id,artist_id,album_id,collection_id)) STORED,
    slot TEXT NOT NULL CHECK(slot IN ('image','cover')),
    file_name TEXT NOT NULL UNIQUE CHECK(file_name ~ '^musicart[0-9]+\.(jpg|png)$'),
    content_type TEXT NOT NULL CHECK(content_type IN ('image/jpeg','image/png')),
    size_bytes BIGINT NOT NULL CHECK(size_bytes BETWEEN 1 AND 8388608),
    width INTEGER NOT NULL CHECK(width BETWEEN 1 AND 1024),
    height INTEGER NOT NULL CHECK(height BETWEEN 1 AND 1024),
    cleanup_job_id BIGINT NOT NULL UNIQUE,
    CHECK(num_nonnulls(asset_id,artist_id,album_id,collection_id)=1),
    FOREIGN KEY(asset_id,owner_id) REFERENCES music_assets(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(artist_id,owner_id) REFERENCES music_artists(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(album_id,owner_id) REFERENCES music_albums(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(collection_id,owner_id) REFERENCES music_collections(id,owner_id) ON DELETE CASCADE,
    UNIQUE(target_kind,target_id,slot)
);
CREATE INDEX music_artworks_owner ON music_artworks(owner_id);
CREATE FUNCTION retire_music_artwork() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE surviving_owner BIGINT;
BEGIN
    SELECT id INTO surviving_owner FROM accounts WHERE id=OLD.owner_id;
    INSERT INTO media_cleanup_jobs(id,file_name) VALUES(OLD.cleanup_job_id,OLD.file_name)
        ON CONFLICT(file_name) DO NOTHING;
    INSERT INTO music_storage_debt(file_name,owner_id,size_bytes)
        VALUES(OLD.file_name,surviving_owner,OLD.size_bytes)
        ON CONFLICT(file_name) DO UPDATE SET size_bytes=GREATEST(music_storage_debt.size_bytes,EXCLUDED.size_bytes);
    RETURN OLD;
END;
$$;
CREATE TRIGGER music_artwork_retirement AFTER DELETE ON music_artworks
    FOR EACH ROW EXECUTE FUNCTION retire_music_artwork();
