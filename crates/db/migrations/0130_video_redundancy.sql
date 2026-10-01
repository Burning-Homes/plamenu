-- Creator permission is separate from ordinary playback/download permission.
ALTER TABLE video_distribution ADD COLUMN mirror_allowed BOOLEAN NOT NULL DEFAULT false;

-- A private operator request cannot become a public CacheFile until all
-- immutable assets have durable storage acknowledgements and authority still
-- matches. Every restart/retry carries an independent generation fence.
CREATE TABLE video_mirrors (
    id BIGINT PRIMARY KEY,
    media_id BIGINT NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    status_id BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    requested_by BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    source_uri TEXT NOT NULL CHECK (length(source_uri) BETWEEN 1 AND 2048),
    source_master_url TEXT NOT NULL CHECK (length(source_master_url) BETWEEN 1 AND 2048),
    source_updated_at TIMESTAMPTZ NOT NULL,
    consent_attestation TEXT NOT NULL DEFAULT '' CHECK (length(consent_attestation) <= 2000),
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','copying','ready','failed','withdrawn')),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    lease_until TIMESTAMPTZ,
    expires_at TIMESTAMPTZ NOT NULL,
    reserved_bytes BIGINT NOT NULL CHECK (reserved_bytes > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    last_error TEXT NOT NULL DEFAULT '' CHECK (length(last_error) <= 500),
    create_activity JSONB,
    CHECK (expires_at > created_at)
);
CREATE UNIQUE INDEX video_mirror_one_active ON video_mirrors(media_id)
    WHERE state IN ('queued','copying','ready');
CREATE INDEX video_mirrors_work ON video_mirrors(state,lease_until,expires_at);

ALTER TABLE media_cleanup_jobs ADD COLUMN mirror_bytes BIGINT NOT NULL DEFAULT 0
    CHECK (mirror_bytes >= 0);
ALTER TABLE media_cleanup_jobs ADD COLUMN mirror_id BIGINT REFERENCES video_mirrors(id) ON DELETE SET NULL;
ALTER TABLE media_cleanup_jobs ADD COLUMN mirror_generation BIGINT CHECK (mirror_generation > 0);

CREATE TABLE video_mirror_approvals (
    domain TEXT PRIMARY KEY CHECK (length(domain) BETWEEN 1 AND 253),
    allow_source BOOLEAN NOT NULL DEFAULT false,
    allow_mirror BOOLEAN NOT NULL DEFAULT false,
    approved_by BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE video_mirror_assets (
    id BIGINT PRIMARY KEY,
    mirror_id BIGINT NOT NULL REFERENCES video_mirrors(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL CHECK (generation > 0),
    source_url TEXT NOT NULL CHECK (length(source_url) BETWEEN 1 AND 2048),
    public_name TEXT NOT NULL CHECK (public_name ~ '^[A-Za-z0-9][A-Za-z0-9_.-]{0,199}$'),
    file_name TEXT NOT NULL UNIQUE,
    bytes BIGINT NOT NULL CHECK (bytes > 0),
    content_type TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[a-f0-9]{64}$'),
    UNIQUE(mirror_id,generation,source_url),
    UNIQUE(mirror_id,generation,public_name)
);

CREATE FUNCTION fence_video_mirror_asset() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    owner video_mirrors;
    used BIGINT;
BEGIN
    SELECT * INTO owner FROM video_mirrors WHERE id=NEW.mirror_id FOR UPDATE;
    IF NOT FOUND OR owner.state<>'copying' OR owner.generation<>NEW.generation
       OR owner.lease_until IS NULL OR owner.lease_until<=clock_timestamp()
       OR owner.expires_at<=clock_timestamp() THEN
        RAISE EXCEPTION 'Stale or expired mirror copy generation';
    END IF;
    SELECT COALESCE(sum(bytes),0) INTO used FROM video_mirror_assets
        WHERE mirror_id=NEW.mirror_id AND generation=NEW.generation;
    IF used+NEW.bytes>owner.reserved_bytes THEN
        RAISE EXCEPTION 'Mirror asset exceeds its reserved quota';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER video_mirror_asset_fence BEFORE INSERT ON video_mirror_assets
    FOR EACH ROW EXECUTE FUNCTION fence_video_mirror_asset();

CREATE FUNCTION retain_mirror_cleanup() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    surviving_mirror BIGINT;
BEGIN
    SELECT id INTO surviving_mirror FROM video_mirrors WHERE id=OLD.mirror_id;
    INSERT INTO media_cleanup_jobs(id,file_name,mirror_bytes,mirror_id,mirror_generation)
    VALUES (OLD.id,OLD.file_name,OLD.bytes,surviving_mirror,OLD.generation)
    ON CONFLICT(file_name) DO UPDATE SET
        mirror_bytes=GREATEST(media_cleanup_jobs.mirror_bytes,EXCLUDED.mirror_bytes);
    RETURN OLD;
END;
$$;
CREATE TRIGGER video_mirror_retirement AFTER DELETE ON video_mirror_assets
    FOR EACH ROW EXECUTE FUNCTION retain_mirror_cleanup();

-- Offers from another authenticated server do not create local copies or
-- grant authority. A URI remains permanently bound to its advertising actor.
CREATE TABLE remote_video_mirrors (
    uri TEXT PRIMARY KEY CHECK (length(uri) BETWEEN 1 AND 2048),
    actor_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_id BIGINT NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    source_uri TEXT NOT NULL CHECK (length(source_uri) BETWEEN 1 AND 2048),
    playlist_url TEXT NOT NULL CHECK (length(playlist_url) BETWEEN 1 AND 2048),
    canonical_playlists BOOLEAN NOT NULL DEFAULT false,
    expires_at TIMESTAMPTZ NOT NULL,
    withdrawn_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX remote_video_mirrors_media ON remote_video_mirrors(media_id,expires_at);

-- Retain bounded actor-bound Undo knowledge even when it arrived before
-- Create or after source/media deletion removed an offer's FK sidecar.
CREATE TABLE remote_video_mirror_tombstones (
    uri TEXT PRIMARY KEY CHECK (length(uri) BETWEEN 1 AND 2048),
    actor_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    withdrawn_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX remote_video_mirror_tombstones_actor ON remote_video_mirror_tombstones(actor_id);
