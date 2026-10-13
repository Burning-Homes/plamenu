ALTER TABLE music_collections ADD COLUMN tags TEXT[] NOT NULL DEFAULT '{}'
    CHECK(music_terms_valid(tags,50,100));
CREATE INDEX music_collections_tags ON music_collections USING GIN(tags);
