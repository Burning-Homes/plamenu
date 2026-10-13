-- A standalone track can have a release year without an album. Preserve the
-- already imported album-derived year when separating the two concepts.
ALTER TABLE music_assets ADD COLUMN release_year INTEGER CHECK (release_year BETWEEN 1 AND 9999);
UPDATE music_assets t SET release_year=a.release_year FROM music_albums a WHERE a.id=t.album_id;
