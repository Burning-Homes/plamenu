-- Playback is not DRM. These flags control explicit download offers and
-- first-party framing, not the ability to retain already-public media bytes.
CREATE TABLE video_distribution (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    download_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    embed_enabled BOOLEAN NOT NULL DEFAULT TRUE
);
