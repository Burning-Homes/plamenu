-- One bounded sampling position per retained source. Directory responses are
-- still untrusted hints; advancing a page never extends actor evidence.
CREATE TABLE discovery_sampling (
    source_id bigint PRIMARY KEY REFERENCES discovery_sources(id) ON DELETE CASCADE,
    round bigint NOT NULL DEFAULT 0 CHECK (round >= 0),
    active_offset integer NOT NULL DEFAULT 0 CHECK (active_offset BETWEEN 0 AND 400),
    new_offset integer NOT NULL DEFAULT 0 CHECK (new_offset BETWEEN 0 AND 400),
    seed bigint NOT NULL DEFAULT hashtextextended(gen_random_uuid()::text,0),
    next_at timestamptz NOT NULL DEFAULT now(),
    pages bigint NOT NULL DEFAULT 0,
    candidates bigint NOT NULL DEFAULT 0
);
INSERT INTO discovery_sampling(source_id) SELECT id FROM discovery_sources;

-- Receipts outlive their jobs, retaining only aggregate operational facts.
ALTER TABLE discovery_requests
    ADD COLUMN family text NOT NULL DEFAULT 'legacy'
        CHECK (family IN ('legacy','source','profile','post','community','video','search')),
    ADD COLUMN operation text NOT NULL DEFAULT 'legacy' CHECK (octet_length(operation)<=32);
CREATE INDEX discovery_requests_family ON discovery_requests(family,started_at);

CREATE FUNCTION discovery_family(op text,target text,cursor_ text,caps text[],kind text)
RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
 SELECT CASE
   WHEN op='list_peers' THEN 'source'
   WHEN op='sample_outbox' OR kind='post' OR cursor_ LIKE 'http%' THEN 'post'
   WHEN kind='video' OR cursor_='video' OR 'video_catalogue'=ANY(caps)
      OR split_part(target,'?',1) LIKE '%/api/v1/videos' THEN 'video'
   WHEN kind='community' OR cursor_='community' OR 'community_directory'=ANY(caps)
      OR split_part(target,'?',1) LIKE '%/api/v3/community/list' THEN 'community'
   ELSE 'profile' END
$$;
-- Approximate wire cost of one useful candidate, not a quality/popularity score.
CREATE FUNCTION discovery_cost(family text) RETURNS integer
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
 SELECT CASE family WHEN 'video' THEN 4 WHEN 'post' THEN 3 WHEN 'community' THEN 2 ELSE 1 END
$$;

-- Keep a stable author-round when entries are removed between cursor requests.
ALTER TABLE discovery_entries ADD COLUMN creator_ordinal bigint NOT NULL DEFAULT 0,
    ADD COLUMN outbox_round bigint NOT NULL DEFAULT 0 CHECK (outbox_round>=0);
WITH ranked AS (
 SELECT id,row_number() OVER (PARTITION BY kind,actor_uri ORDER BY id) AS n
 FROM discovery_entries WHERE kind IN ('post','video')
) UPDATE discovery_entries e SET creator_ordinal=r.n FROM ranked r WHERE e.id=r.id;
CREATE FUNCTION discovery_creator_ordinal() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.kind IN ('post','video') THEN
   -- Discovery entry writes already hold the singleton settings lock.
   NEW.creator_ordinal := COALESCE((SELECT max(creator_ordinal) FROM discovery_entries
      WHERE kind=NEW.kind AND actor_uri=NEW.actor_uri),0)+1;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_creator_ordinal BEFORE INSERT ON discovery_entries
 FOR EACH ROW EXECUTE FUNCTION discovery_creator_ordinal();

-- A decoded <=2 MiB JSON list can legitimately contain more than 32768 hosts.
ALTER TABLE discovery_sources DROP CONSTRAINT discovery_sources_peer_offset_check;
ALTER TABLE discovery_sources ADD CHECK (peer_offset BETWEEN 0 AND 262144);
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;
