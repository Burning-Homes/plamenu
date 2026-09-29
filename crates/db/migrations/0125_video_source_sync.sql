-- Explicit recurring import consent. These are private copies, not follows,
-- caches, publication jobs, or automatic impersonation of a remote creator.
CREATE TABLE video_source_syncs (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel_id BIGINT NOT NULL REFERENCES video_channels(account_id) ON DELETE CASCADE,
    source_url TEXT NOT NULL CHECK (char_length(source_url) BETWEEN 1 AND 2048),
    interval_minutes INTEGER NOT NULL CHECK (interval_minutes BETWEEN 15 AND 10080),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    lease_until TIMESTAMPTZ,
    cursor_url TEXT CHECK (char_length(cursor_url) BETWEEN 1 AND 2048),
    cursor_position INTEGER NOT NULL DEFAULT 0 CHECK (cursor_position BETWEEN 0 AND 100),
    next_check_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_started_at TIMESTAMPTZ,
    last_checked_at TIMESTAMPTZ,
    last_result TEXT NOT NULL DEFAULT 'pending'
        CHECK (last_result IN ('pending','ok','failed','quota','history_limit')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, channel_id, source_url),
    CHECK (enabled OR lease_until IS NULL)
);
CREATE INDEX video_source_syncs_due ON video_source_syncs(next_check_at, id)
    WHERE enabled;

-- Keep deduplication after a copied draft/video is cancelled or deleted. An
-- explicit source removal forgets its history, but never deletes copied media.
CREATE TABLE video_source_sync_items (
    sync_id BIGINT NOT NULL REFERENCES video_source_syncs(id) ON DELETE CASCADE,
    source_uri TEXT NOT NULL CHECK (char_length(source_uri) BETWEEN 1 AND 2048),
    media_id BIGINT REFERENCES media_attachments(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (sync_id, source_uri)
);
CREATE INDEX video_source_sync_items_media ON video_source_sync_items(media_id)
    WHERE media_id IS NOT NULL;
