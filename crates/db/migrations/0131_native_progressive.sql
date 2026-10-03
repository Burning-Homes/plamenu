-- Optional progressive metadata shares the rendition's fenced ownership.
-- Legacy media has no artifact until explicitly regenerated.
ALTER TABLE native_video_hls_renditions ADD COLUMN progressive jsonb;
CREATE UNIQUE INDEX native_progressive_file_name ON native_video_hls_renditions
    ((progressive->>'file_name')) WHERE progressive IS NOT NULL;
CREATE UNIQUE INDEX native_progressive_one_per_media ON native_video_hls_renditions
    (media_id) WHERE progressive IS NOT NULL;
