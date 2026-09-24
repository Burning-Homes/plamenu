-- A draft owns its original for the entire media lifetime, including after
-- publication. Processing claims are generations, not permission to overwrite
-- a stable output key. The worker must reserve unique attempt outputs too.
CREATE TABLE video_drafts (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel_id BIGINT NOT NULL REFERENCES video_channels(account_id) ON DELETE RESTRICT,
    title TEXT NOT NULL CHECK (char_length(title) <= 120),
    description TEXT NOT NULL CHECK (char_length(description) <= 10000),
    language TEXT CHECK (char_length(language) <= 35),
    visibility TEXT NOT NULL CHECK (visibility IN ('public', 'unlisted')),
    source_file TEXT NOT NULL UNIQUE CHECK (source_file ~ '^[a-zA-Z0-9]+\.orig$'),
    source_cleanup_job_id BIGINT NOT NULL UNIQUE,
    source_bytes BIGINT NOT NULL DEFAULT 0 CHECK (source_bytes >= 0),
    reserved_bytes BIGINT NOT NULL CHECK (reserved_bytes > 0 AND reserved_bytes >= source_bytes),
    state TEXT NOT NULL DEFAULT 'receiving'
        CHECK (state IN ('receiving', 'queued', 'processing', 'ready', 'failed')),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    lease_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '7 days',
    CHECK ((state = 'processing') = (lease_until IS NOT NULL)),
    CHECK (state = 'receiving' OR source_bytes > 0)
);
CREATE INDEX video_drafts_owner ON video_drafts(account_id, media_id DESC);
CREATE INDEX video_drafts_queue ON video_drafts(state, lease_until, media_id)
    WHERE state IN ('queued', 'processing');
CREATE INDEX video_drafts_expiry ON video_drafts(expires_at);

CREATE FUNCTION cleanup_video_draft_source() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO media_cleanup_jobs (id, file_name)
    VALUES (OLD.source_cleanup_job_id, OLD.source_file)
    ON CONFLICT (file_name) DO NOTHING;
    RETURN OLD;
END;
$$;
CREATE TRIGGER video_draft_source_cleanup AFTER DELETE ON video_drafts
FOR EACH ROW EXECUTE FUNCTION cleanup_video_draft_source();
