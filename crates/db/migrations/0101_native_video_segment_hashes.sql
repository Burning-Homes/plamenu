-- Immutable HLS byte-range digests are computed while the generation is
-- packaged, then published atomically with the rendition and ordinary MP4.
ALTER TABLE native_video_hls_renditions
    ADD COLUMN segment_sha256 JSONB NOT NULL DEFAULT '{}'::jsonb
    CHECK (jsonb_typeof(segment_sha256) = 'object');

-- Indexed allowlist for the in-process WebRTC tracker. The hash is assigned
-- in the same publication transaction that creates its video UUID.
ALTER TABLE native_video_hls_renditions ADD COLUMN p2p_infohash TEXT UNIQUE
    CHECK (p2p_infohash IS NULL OR length(p2p_infohash) = 20);
