-- A retired principal must not silently turn an admitted job anonymous.
ALTER TABLE music_renditions ADD COLUMN principal_kind TEXT NOT NULL DEFAULT 'anonymous'
    CHECK(principal_kind IN ('anonymous','account','share'));
ALTER TABLE music_renditions ADD COLUMN share_link_id BIGINT
    REFERENCES music_share_links(id) ON DELETE SET NULL;
UPDATE music_renditions SET principal_kind='account' WHERE requester_id IS NOT NULL;
-- Legacy queued/running rows without a surviving account have no durable
-- proof of anonymous admission. Cached artifacts still have current byte ACLs.
UPDATE music_renditions SET state='failed',error='Request principal unavailable',lease=NULL,lease_until=NULL
    WHERE requester_id IS NULL AND state IN ('queued','running');
