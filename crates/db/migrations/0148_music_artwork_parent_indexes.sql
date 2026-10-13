-- FK cascades and parent retirement should not scan unrelated artwork.
CREATE INDEX music_artworks_asset ON music_artworks(asset_id,owner_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_artworks_artist ON music_artworks(artist_id,owner_id) WHERE artist_id IS NOT NULL;
CREATE INDEX music_artworks_album ON music_artworks(album_id,owner_id) WHERE album_id IS NOT NULL;
CREATE INDEX music_artworks_collection ON music_artworks(collection_id,owner_id) WHERE collection_id IS NOT NULL;
