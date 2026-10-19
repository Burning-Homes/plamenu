ALTER TABLE discovery_identity ADD COLUMN hint_changed boolean NOT NULL DEFAULT false;
CREATE TABLE discovery_source_hints (
 endpoint text PRIMARY KEY CHECK(octet_length(endpoint)<=2048),
 introduced_by bigint NOT NULL REFERENCES discovery_sources(id),
 next_at timestamptz NOT NULL DEFAULT now(), created_at timestamptz NOT NULL DEFAULT now()
);
CREATE FUNCTION discovery_clear_hint_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='INSERT' OR NEW.verified_at IS DISTINCT FROM OLD.verified_at THEN
   UPDATE discovery_identity SET hint_changed=false WHERE canonical_uri=NEW.canonical_uri;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_z_clear_hint_change AFTER INSERT OR UPDATE ON discovery_entries FOR EACH ROW EXECUTE FUNCTION discovery_clear_hint_change();
CREATE FUNCTION discovery_resolved_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.uri IS NOT NULL THEN
   INSERT INTO discovery_identity(canonical_uri,kind,outcome,next_check_at)
    VALUES(NEW.uri,CASE TG_TABLE_NAME WHEN 'accounts' THEN 'profile' ELSE 'post' END,'resolved',now()+interval '14 days') ON CONFLICT DO NOTHING;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER discovery_resolved_identity AFTER INSERT ON accounts FOR EACH ROW EXECUTE FUNCTION discovery_resolved_identity();
CREATE TRIGGER discovery_resolved_identity AFTER INSERT ON statuses FOR EACH ROW EXECUTE FUNCTION discovery_resolved_identity();
