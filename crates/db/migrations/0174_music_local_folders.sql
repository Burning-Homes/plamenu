-- Indexed originals stay on operator mounts. Only derived bytes incur storage
-- quota/deletion debt; retiring an index never unlinks an operator original.
ALTER TABLE music_assets ADD COLUMN indexed_original BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE music_assets ADD COLUMN indexed_tags_revision BIGINT CHECK(indexed_tags_revision>0);
ALTER TABLE music_assets DROP CONSTRAINT music_assets_stream_bytes_check;
ALTER TABLE music_assets ADD CONSTRAINT music_assets_stream_bytes_check
    CHECK(stream_bytes>0 OR (indexed_original AND stream_bytes=0));
CREATE TABLE music_local_folders (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    collection_id BIGINT NOT NULL UNIQUE,
    kind TEXT NOT NULL DEFAULT 'library' CHECK(kind='library'),
    root_id TEXT NOT NULL CHECK(length(root_id) BETWEEN 1 AND 64),
    directory TEXT NOT NULL DEFAULT '' CHECK(length(directory)<=2048),
    root_device BIGINT NOT NULL CHECK(root_device>=0),
    root_inode BIGINT NOT NULL CHECK(root_inode>0),
    enabled BOOLEAN NOT NULL DEFAULT true,
    watch BOOLEAN NOT NULL DEFAULT true,
    options JSONB NOT NULL DEFAULT '{}'::jsonb CHECK(jsonb_typeof(options)='object'),
    revision BIGINT NOT NULL DEFAULT 1 CHECK(revision>0),
    scan_epoch BIGINT NOT NULL DEFAULT 1 CHECK(scan_epoch>0),
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN('idle','queued','scanning','indexing','failed')),
    lease TEXT,
    lease_until TIMESTAMPTZ,
    next_scan_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_scan_at TIMESTAMPTZ,
    last_error TEXT CHECK(length(last_error)<=512),
    discovered BIGINT NOT NULL DEFAULT 0 CHECK(discovered>=0),
    indexed BIGINT NOT NULL DEFAULT 0 CHECK(indexed>=0),
    failed BIGINT NOT NULL DEFAULT 0 CHECK(failed>=0),
    CHECK((lease IS NULL)=(lease_until IS NULL)),
    FOREIGN KEY(collection_id,owner_id) REFERENCES music_collections(id,owner_id) ON DELETE CASCADE,
    FOREIGN KEY(collection_id,kind) REFERENCES music_collections(id,kind) ON DELETE CASCADE,
    UNIQUE(owner_id,root_id,directory),
    UNIQUE(id,owner_id)
);
CREATE INDEX music_local_folders_due ON music_local_folders(next_scan_at,id) WHERE enabled;
CREATE TABLE music_local_files (
    id BIGINT PRIMARY KEY,
    folder_id BIGINT NOT NULL,
    owner_id BIGINT NOT NULL,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE SET NULL,
    relative_path TEXT NOT NULL CHECK(length(relative_path) BETWEEN 1 AND 2048),
    original_format TEXT CHECK(original_format IN('mp3','flac','wav','aac','m4a','ogg','mka','wma')),
    device BIGINT NOT NULL CHECK(device>=0),
    inode BIGINT NOT NULL CHECK(inode>0),
    size_bytes BIGINT NOT NULL CHECK(size_bytes>0),
    modified_seconds BIGINT NOT NULL,
    modified_nanos BIGINT NOT NULL CHECK(modified_nanos BETWEEN 0 AND 999999999),
    seen_epoch BIGINT NOT NULL CHECK(seen_epoch>0),
    available BOOLEAN NOT NULL DEFAULT true,
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN('queued','indexing','ready','failed','missing','ignored')),
    lease TEXT,
    lease_until TIMESTAMPTZ,
    last_error TEXT CHECK(length(last_error)<=512),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK((lease IS NULL)=(lease_until IS NULL)),
    FOREIGN KEY(folder_id,owner_id) REFERENCES music_local_folders(id,owner_id) ON DELETE CASCADE,
    UNIQUE(folder_id,relative_path)
);
CREATE INDEX music_local_files_asset ON music_local_files(asset_id,id) WHERE available AND asset_id IS NOT NULL;
CREATE INDEX music_local_files_due ON music_local_files(folder_id,state,id) WHERE available;

-- Explicit native removal is an index tombstone, not permission to delete a
-- mounted original or automatically resurrect it on the next watcher scan.
CREATE FUNCTION music_local_asset_removed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE music_local_files SET state='ignored',asset_id=NULL,lease=NULL,lease_until=NULL
    WHERE asset_id=OLD.id;
    RETURN OLD;
END;
$$;
CREATE TRIGGER music_local_asset_removed BEFORE DELETE ON music_assets
FOR EACH ROW EXECUTE FUNCTION music_local_asset_removed();
