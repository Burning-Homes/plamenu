-- Creation is a member capability; session administration is separately delegated.
UPDATE user_roles SET permissions = permissions | (1::bigint << 22);
UPDATE user_roles SET permissions = permissions | (1::bigint << 23) WHERE id IN (2, 3);

ALTER TABLE webxdc_settings ADD COLUMN total_mb integer NOT NULL DEFAULT 10240
    CHECK (total_mb BETWEEN 1 AND 1048576);

-- One immutable ZIP and one expanded copy per verified package digest.
CREATE TABLE webxdc_packages (
    digest_multibase text PRIMARY KEY,
    bundle_bytes bytea NOT NULL,
    storage_bytes bigint NOT NULL CHECK (storage_bytes >= 0)
);
CREATE TABLE webxdc_package_files (
    digest_multibase text NOT NULL REFERENCES webxdc_packages ON DELETE CASCADE,
    path text NOT NULL,
    media_type text NOT NULL,
    bytes bytea NOT NULL,
    PRIMARY KEY (digest_multibase, path)
);
INSERT INTO webxdc_packages
SELECT DISTINCT ON (s.digest_multibase) s.digest_multibase, s.bundle_bytes,
       octet_length(s.bundle_bytes)::bigint + coalesce((
           SELECT sum(octet_length(f.bytes)) FROM webxdc_files f WHERE f.session_id=s.id
       ), 0)::bigint
FROM webxdc_sessions s ORDER BY s.digest_multibase, s.id;
INSERT INTO webxdc_package_files
SELECT s.digest_multibase, f.path, f.media_type, f.bytes
FROM (SELECT DISTINCT ON (digest_multibase) id, digest_multibase
      FROM webxdc_sessions ORDER BY digest_multibase, id) s
JOIN webxdc_files f ON f.session_id=s.id;
ALTER TABLE webxdc_sessions ADD FOREIGN KEY (digest_multibase) REFERENCES webxdc_packages;
CREATE INDEX idx_webxdc_sessions_package ON webxdc_sessions (digest_multibase);
ALTER TABLE webxdc_sessions DROP COLUMN bundle_bytes;
DROP TABLE webxdc_files;
-- Keep session-scoped reads while the bytes themselves are shared.
CREATE VIEW webxdc_files AS
SELECT s.id AS session_id, f.path, f.media_type, f.bytes
FROM webxdc_sessions s JOIN webxdc_package_files f USING (digest_multibase);

-- Covers explicit deletion, lifecycle expiry and account cascades. Writers
-- take the same lock before adding package references or reserving quota.
CREATE FUNCTION webxdc_release_package() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(752, 1);
    DELETE FROM webxdc_packages p WHERE p.digest_multibase=OLD.digest_multibase
      AND NOT EXISTS (SELECT 1 FROM webxdc_sessions s WHERE s.digest_multibase=p.digest_multibase);
    RETURN NULL;
END;
$$;
CREATE TRIGGER webxdc_release_package AFTER DELETE ON webxdc_sessions
FOR EACH ROW EXECUTE FUNCTION webxdc_release_package();
