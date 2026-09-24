-- Video channels are publishing identities, not discussion communities.
-- Both reuse accounts for signatures, discovery and subscriptions, but their
-- ownership and publication rules are deliberately separate.
CREATE TABLE video_channels (
    account_id BIGINT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    owner_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (account_id <> owner_account_id)
);
CREATE INDEX video_channels_owner ON video_channels(owner_account_id);

-- The status owns social identity/audience; the publication owns media and
-- channel identity. Future broadcast sessions reference this stable status,
-- rather than changing its identity each time an encoder reconnects.
CREATE TABLE video_publications (
    status_id BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    channel_id BIGINT NOT NULL REFERENCES video_channels(account_id) ON DELETE RESTRICT,
    media_id BIGINT NOT NULL UNIQUE REFERENCES media_attachments(id) ON DELETE CASCADE,
    uuid TEXT NOT NULL UNIQUE DEFAULT gen_random_uuid()::text
);
CREATE INDEX video_publications_channel ON video_publications(channel_id, status_id DESC);
