-- Imports are private receiving drafts, not remote playback caches or posts.
-- A restart can only reclaim a fenced download; publication stays explicit.
CREATE TABLE video_source_imports (
    media_id BIGINT PRIMARY KEY REFERENCES video_drafts(media_id) ON DELETE CASCADE,
    source_url TEXT NOT NULL CHECK (char_length(source_url) BETWEEN 1 AND 2048),
    source_kind TEXT NOT NULL CHECK (source_kind IN ('file', 'activitypub')),
    state TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'downloading', 'failed', 'done')),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    lease_until TIMESTAMPTZ,
    CHECK ((state = 'downloading') = (lease_until IS NOT NULL))
);
CREATE INDEX video_source_imports_queue ON video_source_imports(state, lease_until, media_id)
    WHERE state IN ('queued', 'downloading');
