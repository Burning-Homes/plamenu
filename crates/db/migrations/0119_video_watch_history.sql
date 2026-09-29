-- Explicitly opted-in, private local viewer data. Never federated.
CREATE TABLE video_viewer_preferences (
    account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    history_enabled boolean NOT NULL DEFAULT false
);
CREATE TABLE video_watch_history (
    account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_id bigint NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    position_seconds integer NOT NULL CHECK (position_seconds BETWEEN 0 AND 604800),
    completed boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, media_id)
);
CREATE INDEX video_watch_history_recent ON video_watch_history(account_id, updated_at DESC, media_id DESC);
CREATE INDEX video_watch_history_expiry ON video_watch_history(updated_at);
