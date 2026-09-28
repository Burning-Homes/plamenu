-- Channel authority is separate from immutable post authorship. Invitations
-- confer no authority before their recipient explicitly accepts them.
CREATE TABLE video_channel_grants (
    channel_id BIGINT NOT NULL REFERENCES video_channels(account_id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('publisher', 'editor')),
    accepted BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, account_id)
);
CREATE INDEX video_channel_grants_recipient ON video_channel_grants(account_id, channel_id);
ALTER TABLE video_channels ADD COLUMN transfer_to BIGINT REFERENCES accounts(id) ON DELETE SET NULL;
