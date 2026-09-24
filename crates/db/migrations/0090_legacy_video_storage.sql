-- Grandfather the files of M1 native publications that predate durable
-- drafts. The files already exist, so no admission decision is made here;
-- their measured size is charged until physical cleanup acknowledges removal.
-- Unknown sizes block further admission until the startup reconciler stats
-- each key under its media-row fence.
INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT m.file_name, m.account_id, m.id, m.file_size
FROM video_publications v JOIN media_attachments m ON m.id = v.media_id
WHERE m.file_name IS NOT NULL
ON CONFLICT (file_name) DO NOTHING;

INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT m.small_file_name, m.account_id, m.id, m.thumbnail_file_size
FROM video_publications v JOIN media_attachments m ON m.id = v.media_id
WHERE m.small_file_name IS NOT NULL
ON CONFLICT (file_name) DO NOTHING;

INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT p.file_name, m.account_id, m.id, p.file_size
FROM video_compatibility_posters p JOIN media_attachments m ON m.id = p.media_id
ON CONFLICT (file_name) DO NOTHING;

-- A draft can have been deleted before this ledger was introduced while its
-- cleanup job is still pending. Its original/attempt key remains charged to
-- the global quota. Its former owner cannot be recovered after the cascade;
-- account 0 is a sentinel, not an account FK or a quota bypass for new media.
INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT j.file_name, 0, 0, NULL
FROM media_cleanup_jobs j
WHERE j.file_name ~ '^[a-zA-Z0-9]+\.(orig|draft\.(mp4|small\.avif|poster\.jpg))$'
ON CONFLICT (file_name) DO NOTHING;
