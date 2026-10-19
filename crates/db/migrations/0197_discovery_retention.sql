-- Retention is independent of remote recheck scheduling. The legacy
-- valid_until field now means metadata retention; year 9999 means no age limit.
CREATE TABLE discovery_identity (
 canonical_uri text PRIMARY KEY,
 kind text NOT NULL,
 source_id bigint,
 first_discovered_at timestamptz NOT NULL DEFAULT now(),
 last_encountered_at timestamptz NOT NULL DEFAULT now(),
 verified_at timestamptz,
 next_check_at timestamptz NOT NULL DEFAULT now(),
 retained_until timestamptz,
 outcome text NOT NULL DEFAULT 'pending',
 revision bigint NOT NULL DEFAULT 0,
 fingerprint text,
 advertised_version text,
 quiet_checks integer NOT NULL DEFAULT 0,
 forget_after timestamptz
);
CREATE INDEX discovery_identity_due ON discovery_identity(next_check_at) WHERE outcome='active';
CREATE TABLE discovery_aliases (
 alias text PRIMARY KEY, canonical_uri text NOT NULL REFERENCES discovery_identity(canonical_uri) ON DELETE CASCADE
);
CREATE TABLE discovery_daily (
 day date NOT NULL DEFAULT (now() AT TIME ZONE 'UTC')::date,
 origin text NOT NULL, kind text NOT NULL, purpose text NOT NULL,
 event text NOT NULL, total bigint NOT NULL DEFAULT 0,
 PRIMARY KEY(day,origin,kind,purpose,event)
);
CREATE FUNCTION discovery_count(origin_ text,kind_ text,purpose_ text,event_ text,n bigint DEFAULT 1)
RETURNS void LANGUAGE sql AS $$
 INSERT INTO discovery_daily(origin,kind,purpose,event,total) VALUES(origin_,kind_,purpose_,event_,n)
 ON CONFLICT(day,origin,kind,purpose,event) DO UPDATE SET total=discovery_daily.total+excluded.total
$$;
INSERT INTO discovery_identity(canonical_uri,kind,source_id,first_discovered_at,verified_at,next_check_at,retained_until,outcome,revision,fingerprint)
 SELECT canonical_uri,kind,source_id,verified_at,verified_at,
 verified_at+CASE kind WHEN 'community' THEN interval '30 days' WHEN 'video' THEN interval '90 days' WHEN 'post' THEN interval '90 days' ELSE interval '14 days' END,
 CASE kind WHEN 'post' THEN verified_at+interval '180 days' WHEN 'video' THEN verified_at+interval '365 days' ELSE '9999-01-01'::timestamptz END,
 state,revision,md5(concat_ws('|',title,summary,language,discoverable,indexable,visibility)) FROM discovery_entries;
-- Existing resolved identities are dedup evidence, never discovery consent.
INSERT INTO discovery_identity(canonical_uri,kind,outcome,next_check_at)
 SELECT uri,'profile','resolved',now()+interval '14 days' FROM accounts WHERE domain IS NOT NULL
 ON CONFLICT DO NOTHING;
INSERT INTO discovery_identity(canonical_uri,kind,outcome,next_check_at)
 SELECT uri,'post','resolved',now()+interval '90 days' FROM statuses WHERE uri IS NOT NULL
 ON CONFLICT DO NOTHING;
INSERT INTO discovery_identity(canonical_uri,kind,source_id,outcome)
 SELECT target_uri,CASE WHEN cursor='video' THEN 'video' WHEN cursor='community' THEN 'community'
 WHEN cursor LIKE 'http%' THEN 'post' ELSE 'profile' END,source_id,CASE WHEN withdrawn THEN 'withdrawn' ELSE 'pending' END
 FROM discovery_jobs WHERE operation IN ('refresh_entry','revalidate_entry') ON CONFLICT DO NOTHING;
UPDATE discovery_entries e SET valid_until=i.retained_until FROM discovery_identity i WHERE i.canonical_uri=e.canonical_uri;

CREATE FUNCTION discovery_keep_identity() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE first_at timestamptz;
BEGIN
 IF TG_OP='DELETE' THEN
   UPDATE discovery_identity SET outcome=CASE WHEN OLD.state='active' THEN 'retired' ELSE OLD.state END,
      forget_after=CASE WHEN OLD.state='active' THEN now()+interval '365 days' ELSE NULL END
      WHERE canonical_uri=OLD.canonical_uri;
   PERFORM discovery_count(COALESCE((SELECT origin FROM discovery_sources WHERE id=OLD.source_id),'removed'),OLD.kind,'maintenance',
       CASE WHEN OLD.valid_until<=now() THEN 'retention_expired' ELSE 'removed' END);
   RETURN OLD;
 END IF;
 SELECT first_discovered_at INTO first_at FROM discovery_identity WHERE canonical_uri=NEW.canonical_uri AND verified_at IS NOT NULL;
 first_at:=COALESCE(first_at,NEW.verified_at);
 NEW.valid_until:=CASE NEW.kind WHEN 'post' THEN first_at+interval '180 days' WHEN 'video' THEN first_at+interval '365 days' ELSE '9999-01-01'::timestamptz END;
 IF TG_OP='UPDATE' AND OLD.published_at IS NOT NULL AND NEW.state='active' THEN NEW.published_at:=OLD.published_at; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_keep_identity BEFORE INSERT OR UPDATE OR DELETE ON discovery_entries FOR EACH ROW EXECUTE FUNCTION discovery_keep_identity();
CREATE FUNCTION discovery_entry_event() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE signature text; old_signature text; quiet integer; delay interval; first_at timestamptz;
BEGIN
 IF TG_OP='UPDATE' AND ROW(NEW.verified_at,NEW.state,NEW.discoverable,NEW.indexable,NEW.visibility,NEW.title,NEW.summary)
   IS NOT DISTINCT FROM ROW(OLD.verified_at,OLD.state,OLD.discoverable,OLD.indexable,OLD.visibility,OLD.title,OLD.summary) THEN RETURN NEW; END IF;
 SELECT fingerprint,quiet_checks,CASE WHEN verified_at IS NOT NULL THEN first_discovered_at END
   INTO old_signature,quiet,first_at FROM discovery_identity WHERE canonical_uri=NEW.canonical_uri;
 signature:=md5(concat_ws('|',NEW.title,NEW.summary,NEW.language,NEW.discoverable,NEW.indexable,NEW.visibility));
 quiet:=CASE WHEN signature=old_signature THEN LEAST(COALESCE(quiet,0)+1,3) ELSE 0 END;
 delay:=LEAST(interval '90 days',CASE NEW.kind WHEN 'community' THEN interval '30 days' WHEN 'video' THEN interval '90 days' WHEN 'post' THEN interval '90 days' ELSE interval '14 days' END * (1+quiet));
 INSERT INTO discovery_identity(canonical_uri,kind,source_id,first_discovered_at,verified_at,next_check_at,retained_until,outcome,revision,fingerprint,quiet_checks)
 VALUES(NEW.canonical_uri,NEW.kind,NEW.source_id,COALESCE(first_at,NEW.verified_at),NEW.verified_at,NEW.verified_at+delay,NEW.valid_until,NEW.state,NEW.revision,signature,quiet)
 ON CONFLICT(canonical_uri) DO UPDATE SET kind=excluded.kind,source_id=excluded.source_id,first_discovered_at=excluded.first_discovered_at,verified_at=excluded.verified_at,
 next_check_at=excluded.next_check_at,retained_until=excluded.retained_until,outcome=excluded.outcome,revision=excluded.revision,
 fingerprint=excluded.fingerprint,quiet_checks=excluded.quiet_checks,forget_after=NULL;
 PERFORM discovery_count(COALESCE((SELECT origin FROM discovery_sources WHERE id=NEW.source_id),'removed'),NEW.kind,
 CASE WHEN TG_OP='INSERT' THEN 'acquisition' ELSE 'maintenance' END,
 CASE WHEN NEW.state<>'active' THEN 'refused' WHEN TG_OP='INSERT' THEN 'added'
 WHEN signature IS DISTINCT FROM old_signature THEN 'updated' ELSE 'unchanged' END);
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_entry_event AFTER INSERT OR UPDATE ON discovery_entries FOR EACH ROW EXECUTE FUNCTION discovery_entry_event();

ALTER TABLE discovery_sampling ADD COLUMN zero_yield integer NOT NULL DEFAULT 0;
CREATE TABLE discovery_seen (
 account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 entry_id bigint NOT NULL REFERENCES discovery_entries(id) ON DELETE CASCADE,
 seen_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(account_id,entry_id)
);
CREATE INDEX discovery_seen_expiry ON discovery_seen(seen_at);
-- Bounded reusable evidence, not full actor documents or imported accounts.
CREATE TABLE discovery_actor_evidence (
 uri text PRIMARY KEY, document jsonb NOT NULL CHECK(octet_length(document::text)<=8192),
 verified_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE discovery_outbox_walk (
 actor_uri text PRIMARY KEY, next_uri text, zero_yield integer NOT NULL DEFAULT 0, rounds integer NOT NULL DEFAULT 0
);
ALTER TABLE discovery_requests ADD COLUMN purpose text NOT NULL DEFAULT 'legacy';
CREATE FUNCTION discovery_request_purpose() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.purpose='legacy' THEN NEW.purpose:=CASE WHEN NEW.exploration THEN 'exploration' ELSE 'acquisition' END; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_request_purpose BEFORE INSERT ON discovery_requests FOR EACH ROW EXECUTE FUNCTION discovery_request_purpose();
-- Leave a self-contained audit even if an administrator later removes a source.
CREATE FUNCTION discovery_source_removed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN PERFORM discovery_count(OLD.origin,'source','maintenance','source_removed'); RETURN OLD; END $$;
CREATE TRIGGER discovery_source_removed BEFORE DELETE ON discovery_sources FOR EACH ROW EXECUTE FUNCTION discovery_source_removed();
ALTER TABLE discovery_settings ALTER COLUMN max_entries SET DEFAULT 100000;
ALTER TABLE discovery_settings ALTER COLUMN entries_per_origin SET DEFAULT 10000;
ALTER TABLE discovery_settings ALTER COLUMN entries_per_introducer SET DEFAULT 100000;
UPDATE discovery_settings SET max_entries=CASE WHEN max_entries=200 THEN 100000 ELSE max_entries END,
 entries_per_origin=CASE WHEN entries_per_origin=40 THEN 10000 ELSE entries_per_origin END,
 entries_per_introducer=CASE WHEN entries_per_introducer=100 THEN 100000 ELSE entries_per_introducer END,
 revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL,
 exploration=(operation='list_peers' OR (operation='inspect_source' AND source_id IN (SELECT id FROM discovery_sources WHERE verified_at IS NULL)));
CREATE TABLE discovery_candidates (
 uri text PRIMARY KEY CHECK(octet_length(uri)<=2048),
 source_id bigint NOT NULL REFERENCES discovery_sources(id), cursor text NOT NULL,
 video jsonb, created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX discovery_candidates_source ON discovery_candidates(source_id);
CREATE TABLE discovery_pacing (
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 tokens double precision NOT NULL DEFAULT 42, refilled_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO discovery_pacing(singleton) VALUES(true);
CREATE FUNCTION discovery_request_event() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='INSERT' THEN
   PERFORM discovery_count(NEW.origin,NEW.family,NEW.purpose,'wire_attempt');
 ELSIF NOT OLD.completed AND NEW.completed THEN
   PERFORM discovery_count(NEW.origin,NEW.family,NEW.purpose,'charged_bytes',NEW.bytes);
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_request_event AFTER INSERT OR UPDATE ON discovery_requests FOR EACH ROW EXECUTE FUNCTION discovery_request_event();
ALTER TABLE discovery_identity ADD COLUMN activity_at timestamptz;
CREATE TABLE discovery_journeys (
 id bigint PRIMARY KEY, account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 context text NOT NULL, entries bigint[] NOT NULL CHECK(cardinality(entries)<=2000),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX discovery_journeys_owner ON discovery_journeys(account_id,created_at);
CREATE INDEX discovery_entries_recent ON discovery_entries(kind,id DESC) WHERE state='active';
CREATE INDEX discovery_entries_published ON discovery_entries(kind,published_at DESC NULLS LAST,id DESC) WHERE state='active';
CREATE INDEX discovery_entries_shuffle ON discovery_entries(kind,hashtextextended(canonical_uri,0)) WHERE state='active';
CREATE INDEX discovery_entries_actor_published ON discovery_entries(actor_uri,published_at DESC) WHERE state='active';
