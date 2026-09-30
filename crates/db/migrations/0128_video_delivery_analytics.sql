-- Delivery aggregates, not viewer tracking or unique-view estimates.
CREATE TABLE video_analytics_policies (
    channel_id BIGINT PRIMARY KEY REFERENCES video_channels(account_id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT false,
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    retention_days INTEGER NOT NULL DEFAULT 30 CHECK (retention_days BETWEEN 1 AND 90)
);
CREATE TABLE video_delivery_daily (
    channel_id BIGINT NOT NULL REFERENCES video_analytics_policies(channel_id) ON DELETE CASCADE,
    media_id BIGINT NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    day DATE NOT NULL,
    lane TEXT NOT NULL CHECK (lane IN ('file', 'hls', 'live')),
    requests BIGINT NOT NULL CHECK (requests >= 0),
    body_bytes BIGINT NOT NULL CHECK (body_bytes >= 0),
    PRIMARY KEY (channel_id, media_id, day, lane)
);
CREATE INDEX video_delivery_daily_expiry ON video_delivery_daily(day);
CREATE INDEX video_analytics_enabled ON video_analytics_policies(channel_id) WHERE enabled;
