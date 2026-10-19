ALTER TABLE discovery_source_hints ALTER COLUMN introduced_by DROP NOT NULL;
ALTER TABLE discovery_source_hints ADD COLUMN previous_source jsonb;
CREATE FUNCTION discovery_park_unverified(initial_ boolean DEFAULT false) RETURNS void LANGUAGE plpgsql AS $$
DECLARE candidate discovery_sources;
BEGIN
 FOR candidate IN SELECT s.* FROM discovery_sources s WHERE s.provenance='peer' AND s.verified_at IS NULL
   AND (initial_ OR s.last_failure_at IS NOT NULL AND s.cooldown_until>now())
   AND NOT EXISTS(SELECT 1 FROM discovery_entries e WHERE e.source_id=s.id)
   AND NOT EXISTS(SELECT 1 FROM discovery_candidates c WHERE c.source_id=s.id)
   AND NOT EXISTS(SELECT 1 FROM discovery_sources child WHERE child.introduced_by=s.id)
   AND NOT EXISTS(SELECT 1 FROM discovery_source_hints h WHERE h.introduced_by=s.id)
   AND NOT EXISTS(SELECT 1 FROM discovery_jobs j WHERE (j.source_id=s.id AND (j.withdrawn OR j.leased_until>now() OR j.operation NOT IN ('inspect_source','list_peers'))) OR (j.introduced_by=s.id AND j.source_id<>s.id))
   ORDER BY s.id LIMIT 100
 LOOP
   IF (SELECT count(*) FROM discovery_source_hints)>=1000 THEN EXIT; END IF;
   INSERT INTO discovery_source_hints(endpoint,introduced_by,next_at,created_at,previous_source)
    VALUES(candidate.endpoint,candidate.introduced_by,GREATEST(now(),candidate.cooldown_until),candidate.created_at,to_jsonb(candidate))
    ON CONFLICT(endpoint) DO UPDATE SET next_at=GREATEST(discovery_source_hints.next_at,excluded.next_at),previous_source=excluded.previous_source;
   DELETE FROM discovery_jobs WHERE source_id=candidate.id;
   PERFORM discovery_count(candidate.origin,'source','exploration','unverified_hint_parked');
   DELETE FROM discovery_sources WHERE id=candidate.id;
 END LOOP;
END $$;
SELECT discovery_park_unverified(true);
