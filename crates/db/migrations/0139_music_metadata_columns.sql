-- Fixed-shape music data follows the same typed-column contract as other
-- application sidecars. Preserve publications from the initial Audio schema.
ALTER TABLE status_music
    ADD COLUMN artist TEXT NOT NULL DEFAULT '' CHECK (char_length(artist) <= 200),
    ADD COLUMN album TEXT NOT NULL DEFAULT '' CHECK (char_length(album) <= 200),
    ADD COLUMN genre TEXT NOT NULL DEFAULT '' CHECK (char_length(genre) <= 200),
    ADD COLUMN release_year INTEGER CHECK (release_year BETWEEN 1 AND 9999),
    ADD COLUMN track_number INTEGER CHECK (track_number BETWEEN 1 AND 9999),
    ADD COLUMN disc_number INTEGER CHECK (disc_number BETWEEN 1 AND 9999),
    ADD COLUMN musicbrainz_id TEXT;
UPDATE status_music SET
    artist = COALESCE(metadata->>'artist', ''),
    album = COALESCE(metadata->>'album', ''),
    genre = COALESCE(metadata->>'genre', ''),
    release_year = CASE WHEN (metadata->>'release_year')::integer BETWEEN 1 AND 9999 THEN (metadata->>'release_year')::integer END,
    track_number = CASE WHEN (metadata->>'track_number')::integer BETWEEN 1 AND 9999 THEN (metadata->>'track_number')::integer END,
    disc_number = CASE WHEN (metadata->>'disc_number')::integer BETWEEN 1 AND 9999 THEN (metadata->>'disc_number')::integer END,
    musicbrainz_id = metadata->>'musicbrainz_id';
ALTER TABLE status_music DROP COLUMN metadata;
