-- Durable bounded exploration, independent of member activity.
ALTER TABLE discovery_sources
    ADD COLUMN depth smallint NOT NULL DEFAULT 0 CHECK (depth BETWEEN 0 AND 2),
    ADD COLUMN provenance text NOT NULL DEFAULT 'operator' CHECK (provenance IN ('operator','bundled','import','peer')),
    ADD COLUMN peer_next_at timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN peer_offset integer NOT NULL DEFAULT 0 CHECK (peer_offset BETWEEN 0 AND 32768),
    ADD COLUMN peers_verified_at timestamptz,
    ADD COLUMN peer_error text CHECK (octet_length(peer_error) <= 128);
ALTER TABLE discovery_jobs ADD COLUMN exploration boolean NOT NULL DEFAULT false;
UPDATE discovery_jobs j SET exploration = j.operation='list_peers'
    OR (j.operation='inspect_source' AND EXISTS (SELECT 1 FROM discovery_sources s WHERE s.id=j.source_id AND s.verified_at IS NULL))
    OR (j.operation='refresh_entry' AND NOT EXISTS (SELECT 1 FROM discovery_entries e WHERE e.canonical_uri=j.target_uri));
ALTER TABLE discovery_requests ADD COLUMN exploration boolean NOT NULL DEFAULT false;
CREATE TABLE discovery_scheduler (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    turn bigint NOT NULL DEFAULT 0 CHECK (turn BETWEEN 0 AND 4)
);
INSERT INTO discovery_scheduler DEFAULT VALUES;
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;

-- Persistent positions do not change when rotation removes an older card.
ALTER TABLE discovery_sources ADD COLUMN next_ordinal bigint NOT NULL DEFAULT 0;
ALTER TABLE discovery_entries ADD COLUMN browse_ordinal bigint NOT NULL DEFAULT 0;
WITH ranked AS (
    SELECT e.id,row_number() OVER (PARTITION BY s.origin ORDER BY e.id) AS ordinal
    FROM discovery_entries e JOIN discovery_sources s ON s.id=e.source_id
) UPDATE discovery_entries e SET browse_ordinal=r.ordinal FROM ranked r WHERE e.id=r.id;
UPDATE discovery_sources s SET next_ordinal=COALESCE((SELECT max(e.browse_ordinal)
    FROM discovery_entries e JOIN discovery_sources source ON source.id=e.source_id
    WHERE source.origin=s.origin),0);
