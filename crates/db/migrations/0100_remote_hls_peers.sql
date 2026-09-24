-- Federation announces P2P discovery and the segment checksum manifest with
-- the HLS playlist. Keep these bounded hints with the remote attachment, not
-- with a post revision: retranscoding can replace them without editing text.
CREATE TABLE remote_hls_peers (
    media_id BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    video_uuid TEXT NOT NULL CHECK (length(video_uuid) = 36),
    trackers TEXT[] NOT NULL CHECK (cardinality(trackers) BETWEEN 1 AND 8),
    sha256_url TEXT NOT NULL
);
