-- Explicit, revocable contents access derives from the live parent and entry.
-- Existing collections continue to preserve every source's own permissions.
ALTER TABLE music_collections ADD COLUMN share_tracks BOOLEAN NOT NULL DEFAULT false;
CREATE INDEX music_collection_entries_asset_parent
    ON music_collection_entries(asset_id,collection_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_collection_grants_member_parent
    ON music_collection_grants(account_id,collection_id);
