-- Existing catalogue IDs encode their original admission time. Recover that
-- baseline instead of treating the last pre-upgrade verification as discovery.
UPDATE discovery_identity i SET first_discovered_at=LEAST(e.verified_at,to_timestamp((e.id>>16)::double precision/1000))
 FROM discovery_entries e WHERE i.canonical_uri=e.canonical_uri
 AND e.id>82723061760000000; -- IDs minted after 2010; synthetic tiny IDs excluded.
UPDATE discovery_entries SET valid_until=valid_until; -- retention trigger uses restored baseline
CREATE FUNCTION discovery_unsettled_body() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT OLD.completed THEN PERFORM discovery_count(OLD.origin,OLD.family,OLD.purpose,'unsettled_body_reservation',OLD.bytes); END IF;
 RETURN OLD;
END $$;
CREATE TRIGGER discovery_unsettled_body BEFORE DELETE ON discovery_requests FOR EACH ROW EXECUTE FUNCTION discovery_unsettled_body();
