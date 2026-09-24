-- Timed chapters for native and remote video. The sidecar belongs to media so
-- an attachment deletion revokes the chapter metadata with the video.
CREATE TABLE video_chapters (
    media_id bigint NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    start_seconds integer NOT NULL CHECK (start_seconds >= 0 AND start_seconds <= 604800),
    title varchar(200) NOT NULL CHECK (length(trim(title)) > 0),
    PRIMARY KEY (media_id, start_seconds)
);
