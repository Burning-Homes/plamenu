-- A shared name must never allow one member to publish another member's
-- private artist/album metadata through a retained track reference.
ALTER TABLE music_artists ADD CONSTRAINT music_artists_owned_id UNIQUE(id,owner_id);
ALTER TABLE music_albums ADD CONSTRAINT music_albums_owned_id UNIQUE(id,owner_id);
ALTER TABLE music_albums ADD CONSTRAINT music_albums_owned_artist
    FOREIGN KEY(artist_id,owner_id) REFERENCES music_artists(id,owner_id);
ALTER TABLE music_assets ADD CONSTRAINT music_assets_owned_artist
    FOREIGN KEY(artist_id,owner_id) REFERENCES music_artists(id,owner_id);
ALTER TABLE music_assets ADD CONSTRAINT music_assets_owned_album
    FOREIGN KEY(album_id,owner_id) REFERENCES music_albums(id,owner_id);
