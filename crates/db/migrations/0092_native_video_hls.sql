-- Native HLS is published atomically with the compatibility MP4. Playlists
-- live in the database (small, transactional metadata); their single-file
-- fMP4 bodies are immutable attempt outputs covered by the storage journal.
ALTER TABLE video_draft_outputs DROP CONSTRAINT video_draft_outputs_file_name_check;
ALTER TABLE video_draft_outputs ADD CONSTRAINT video_draft_outputs_file_name_check
    CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif|poster\.jpg|hls[0-9]+\.m4s)$');

CREATE TABLE native_video_hls_renditions (
    media_id BIGINT NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL CHECK (generation > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    width INTEGER NOT NULL CHECK (width > 0),
    bandwidth BIGINT NOT NULL CHECK (bandwidth > 0),
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    file_name TEXT NOT NULL UNIQUE REFERENCES video_draft_outputs(file_name) ON DELETE CASCADE,
    playlist TEXT NOT NULL CHECK (length(playlist) BETWEEN 20 AND 4194304),
    PRIMARY KEY (media_id, height)
);
