-- Progressive metadata is fixed-shape application data. Keep its wire JSON
-- projection at readers, and preserve each existing rendition's ownership.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM native_video_hls_renditions
        WHERE progressive IS NOT NULL AND (
            jsonb_typeof(progressive) IS DISTINCT FROM 'object'
            OR progressive - ARRAY['file_name','size_bytes','sha256','width','height',
                 'duration','frame_rate','bitrate','audio_codec'] <> '{}'::jsonb
            OR jsonb_typeof(progressive->'file_name') IS DISTINCT FROM 'string'
            OR jsonb_typeof(progressive->'sha256') IS DISTINCT FROM 'string'
            OR jsonb_typeof(progressive->'height') IS DISTINCT FROM 'number'
            OR jsonb_typeof(progressive->'width') IS DISTINCT FROM 'number'
            OR jsonb_typeof(progressive->'size_bytes') IS DISTINCT FROM 'number'
            OR jsonb_typeof(progressive->'duration') IS DISTINCT FROM 'number'
            OR jsonb_typeof(progressive->'bitrate') IS DISTINCT FROM 'number'
            OR COALESCE(jsonb_typeof(progressive->'frame_rate'),'null') NOT IN ('string','null')
            OR COALESCE(jsonb_typeof(progressive->'audio_codec'),'null') NOT IN ('string','null')
            OR (progressive->>'height')::integer IS DISTINCT FROM height
        )
    ) THEN
        RAISE EXCEPTION 'Progressive metadata has unsupported fields or mismatched rendition ownership';
    END IF;
END;
$$;

CREATE TABLE native_video_progressive_files (
    media_id BIGINT PRIMARY KEY,
    generation BIGINT NOT NULL CHECK (generation > 0),
    height INTEGER NOT NULL CHECK (height > 0),
    width INTEGER NOT NULL CHECK (width > 0),
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    file_name TEXT NOT NULL UNIQUE REFERENCES video_draft_outputs(file_name) ON DELETE CASCADE,
    sha256 TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    duration DOUBLE PRECISION NOT NULL CHECK (duration > 0 AND duration < 'Infinity'::double precision),
    frame_rate TEXT,
    bitrate BIGINT NOT NULL CHECK (bitrate > 0),
    audio_codec TEXT CHECK (audio_codec = 'aac'),
    FOREIGN KEY (media_id, height) REFERENCES native_video_hls_renditions(media_id, height) ON DELETE CASCADE,
    CHECK (file_name ~ ('^[a-zA-Z0-9]+\.draft\.progressive' || height || '\.mp4$'))
);

-- Fail transactionally on malformed legacy metadata, rather than silently
-- dropping it. The old JSON and indexes survive a failed migration.
INSERT INTO native_video_progressive_files
    (media_id, generation, height, width, size_bytes, file_name, sha256,
     duration, frame_rate, bitrate, audio_codec)
SELECT media_id, generation, (progressive->>'height')::integer,
       (progressive->>'width')::integer, (progressive->>'size_bytes')::bigint,
       progressive->>'file_name', progressive->>'sha256',
       (progressive->>'duration')::double precision, progressive->>'frame_rate',
       (progressive->>'bitrate')::bigint, progressive->>'audio_codec'
FROM native_video_hls_renditions WHERE progressive IS NOT NULL;

ALTER TABLE native_video_hls_renditions DROP COLUMN progressive;
