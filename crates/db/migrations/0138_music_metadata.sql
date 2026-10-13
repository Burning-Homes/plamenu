-- Audio metadata is a sidecar: all identity, deletion and access decisions
-- continue to use statuses and their existing attachment authorization.
CREATE TABLE status_music (
    status_id BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    metadata JSONB NOT NULL CHECK (jsonb_typeof(metadata) = 'object')
);
CREATE INDEX statuses_audio_catalogue ON statuses(id DESC)
    WHERE object_type = 'Audio' AND deleted_at IS NULL AND reblog_of_id IS NULL;
