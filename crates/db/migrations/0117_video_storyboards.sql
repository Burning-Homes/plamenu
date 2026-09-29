CREATE TABLE video_storyboards (
    media_id bigint PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    generation bigint NOT NULL CHECK (generation > 0),
    file_name text NOT NULL UNIQUE CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.storyboard\.jpg$'),
    size_bytes bigint NOT NULL CHECK (size_bytes BETWEEN 1 AND 2097152),
    columns integer NOT NULL CHECK (columns BETWEEN 1 AND 10),
    rows integer NOT NULL CHECK (rows BETWEEN 1 AND 10),
    tile_count integer NOT NULL CHECK (tile_count BETWEEN 1 AND 100 AND tile_count <= columns * rows),
    interval_seconds integer NOT NULL CHECK (interval_seconds BETWEEN 1 AND 604800)
);
