-- Music containers retain ordered references, independently of source ACLs.
CREATE TABLE music_collections (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description)<=4000),
    kind TEXT NOT NULL CHECK (kind IN ('library','playlist')),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','local','public')),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX music_collections_owner ON music_collections(owner_id,id DESC);
CREATE INDEX music_collections_public ON music_collections(id DESC) WHERE visibility='public';
CREATE TABLE music_collection_entries (
    id BIGINT PRIMARY KEY,
    collection_id BIGINT NOT NULL REFERENCES music_collections(id) ON DELETE CASCADE,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position BETWEEN 1 AND 10000),
    CHECK (num_nonnulls(asset_id,status_id)=1),
    UNIQUE(collection_id,position) DEFERRABLE INITIALLY DEFERRED
);
CREATE TABLE music_collection_grants (
    collection_id BIGINT NOT NULL REFERENCES music_collections(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY(collection_id,account_id)
);

-- Deleting a source via FK cascade changes the ordered container too. Bump
-- each affected collection once per statement, including multi-track purges.
CREATE FUNCTION music_collection_entries_removed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE music_collections SET revision=revision+1,updated_at=now()
    WHERE id IN (SELECT collection_id FROM removed_entries);
    RETURN NULL;
END;
$$;
CREATE TRIGGER music_collection_entries_removed AFTER DELETE ON music_collection_entries
REFERENCING OLD TABLE AS removed_entries FOR EACH STATEMENT
EXECUTE FUNCTION music_collection_entries_removed();
