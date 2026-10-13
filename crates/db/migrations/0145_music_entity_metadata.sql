-- Reusable entity metadata is owner-scoped; a catalogue entity is not an actor.
-- The API bounds and normalizes each list before writing it, and storage also
-- rejects overlong values regardless of the caller.
CREATE FUNCTION music_terms_valid(terms TEXT[], max_items INTEGER, max_length INTEGER)
RETURNS BOOLEAN LANGUAGE SQL IMMUTABLE STRICT AS $$
    SELECT cardinality(terms)<=max_items AND COALESCE(
        (SELECT bool_and(term IS NOT NULL AND char_length(term) BETWEEN 1 AND max_length)
         FROM unnest(terms) term), true)
$$;
ALTER TABLE music_artists
    ADD COLUMN description TEXT NOT NULL DEFAULT '' CHECK (char_length(description)<=4000),
    ADD COLUMN musicbrainz_id TEXT CHECK (musicbrainz_id ~* '^[a-f0-9]{8}(-[a-f0-9]{4}){3}-[a-f0-9]{12}$'),
    ADD COLUMN tags TEXT[] NOT NULL DEFAULT '{}' CHECK (music_terms_valid(tags,50,100)),
    ADD COLUMN revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_albums
    ADD COLUMN description TEXT NOT NULL DEFAULT '' CHECK (char_length(description)<=4000),
    ADD COLUMN musicbrainz_id TEXT CHECK (musicbrainz_id ~* '^[a-f0-9]{8}(-[a-f0-9]{4}){3}-[a-f0-9]{12}$'),
    ADD COLUMN tags TEXT[] NOT NULL DEFAULT '{}' CHECK (music_terms_valid(tags,50,100)),
    ADD COLUMN genres TEXT[] NOT NULL DEFAULT '{}' CHECK (music_terms_valid(genres,50,100)),
    ADD COLUMN revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_assets
    ADD COLUMN tags TEXT[] NOT NULL DEFAULT '{}' CHECK (music_terms_valid(tags,50,100)),
    ADD COLUMN extra_artists TEXT[] NOT NULL DEFAULT '{}' CHECK (music_terms_valid(extra_artists,20,200));
CREATE INDEX music_assets_artist ON music_assets(artist_id,id);
CREATE INDEX music_assets_album_order ON music_assets(album_id,disc_number,track_number,id);
