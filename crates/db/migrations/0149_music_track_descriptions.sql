-- Retained notes own their caption source and preserve its existing post format.
ALTER TABLE music_assets
    ADD COLUMN description TEXT NOT NULL DEFAULT '',
    ADD COLUMN description_type TEXT NOT NULL DEFAULT 'text/plain'
        CHECK(description_type IN ('text/plain','text/markdown','text/html'));
-- Existing published captions must survive the next metadata/artwork refresh.
-- Application validation bounds newly supplied descriptions; a legacy caption
-- is copied intact rather than truncated or erased on the next source refresh.
UPDATE music_assets t SET description=s.text,description_type=s.content_type
FROM music_publications p JOIN statuses s ON s.id=p.status_id
WHERE p.asset_id=t.id AND s.deleted_at IS NULL
AND s.content_type IN ('text/plain','text/markdown','text/html');
CREATE INDEX music_albums_artist ON music_albums(artist_id,id);
