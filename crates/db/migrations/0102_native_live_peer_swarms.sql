-- A live episode uses the stable Video UUID as its PeerTube-compatible swarm
-- name, but the allowlist is active only while that generation is on air.
ALTER TABLE native_live_sessions ADD COLUMN p2p_infohash TEXT
    CHECK (p2p_infohash IS NULL OR length(p2p_infohash) = 20);
CREATE INDEX native_live_sessions_p2p_infohash_idx
    ON native_live_sessions (p2p_infohash)
    WHERE ended_at IS NULL AND p2p_infohash IS NOT NULL;
