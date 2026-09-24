-- PeerTube and other live publishers can announce several future starts.
-- Keep their calendar metadata separate from the attachment's media bytes so
-- an Update can replace it without recreating the attachment or edit history.
CREATE TABLE remote_live_schedules (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    starts_at TIMESTAMPTZ[] NOT NULL CHECK (cardinality(starts_at) BETWEEN 1 AND 16)
);
