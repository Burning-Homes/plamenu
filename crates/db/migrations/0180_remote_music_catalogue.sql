-- Anonymous catalogue records are distinct from published ActivityPub objects.
-- No credentials, provider state or audio bytes are retained.
ALTER TABLE music_remote_resources
    ADD COLUMN catalogue_kind TEXT CHECK(catalogue_kind IN ('track','stream')),
    ADD COLUMN catalogue_key TEXT CHECK(length(catalogue_key)<=128),
    ADD CONSTRAINT music_remote_catalogue_identity CHECK(
        (catalogue_kind IS NULL AND catalogue_key IS NULL) OR
        (catalogue_kind IS NOT NULL AND catalogue_key IS NOT NULL)
    );
