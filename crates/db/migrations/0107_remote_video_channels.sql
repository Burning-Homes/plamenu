-- A remote Video retains its human author in statuses.account_id, while a
-- verified Group attribution supplies its subscribable video-channel identity.
CREATE TABLE remote_video_channels (
    status_id BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    channel_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE
);
CREATE INDEX remote_video_channels_channel ON remote_video_channels(channel_id, status_id DESC);
