-- A local live has no completed file while waiting. The attachment is still
-- owned, private until publication, and typed as a Video for first-party and
-- generic client presentation.
ALTER TABLE media_attachments DROP CONSTRAINT media_attachments_check;
ALTER TABLE media_attachments ADD CONSTRAINT media_attachments_check
    CHECK (file_name IS NOT NULL OR remote_url IS NOT NULL OR live_state IS NOT NULL);

-- The status and channel remain stable across reconnects. The ingest secret
-- never enters the database in plaintext; a public broadcast UUID is not an
-- authorization credential. A NULL status marks a private, not-yet-announced
-- allocation; publication binds it in the same transaction as Create(Video).
CREATE TABLE native_live_broadcasts (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    status_id BIGINT UNIQUE REFERENCES video_publications(status_id) ON DELETE CASCADE,
    owner_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel_id BIGINT NOT NULL REFERENCES video_channels(account_id) ON DELETE CASCADE,
    broadcast_uuid TEXT NOT NULL UNIQUE DEFAULT gen_random_uuid()::text,
    ingest_key_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(ingest_key_hash) = 32),
    key_generation BIGINT NOT NULL DEFAULT 1 CHECK (key_generation > 0),
    state TEXT NOT NULL DEFAULT 'waiting'
        CHECK (state IN ('waiting', 'live', 'interrupted', 'ended')),
    permanent BOOLEAN NOT NULL DEFAULT false,
    scheduled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at TIMESTAMPTZ,
    ended_at TIMESTAMPTZ,
    CHECK (status_id IS NOT NULL OR state = 'waiting')
);
CREATE INDEX native_live_broadcasts_owner
    ON native_live_broadcasts(owner_account_id, created_at DESC);

-- One on-air episode has an independent identity for future chat, recording,
-- replay and moderation. Reconnecting creates another generation without
-- changing the Video or broadcast identifiers.
CREATE TABLE native_live_sessions (
    session_id TEXT PRIMARY KEY DEFAULT gen_random_uuid()::text,
    media_id BIGINT NOT NULL REFERENCES native_live_broadcasts(media_id) ON DELETE CASCADE,
    generation BIGINT NOT NULL CHECK (generation > 0),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at TIMESTAMPTZ,
    replay_media_id BIGINT REFERENCES media_attachments(id) ON DELETE SET NULL,
    UNIQUE (media_id, generation)
);
CREATE UNIQUE INDEX native_live_sessions_one_active
    ON native_live_sessions(media_id) WHERE ended_at IS NULL;
