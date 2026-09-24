-- Deliberately independent of media/account cascades: deleted database rows
-- do not mean their stored bytes have been removed. Refund only after the
-- storage backend acknowledges deletion. Account ids are accounting identities,
-- not foreign keys whose deletion could erase an outstanding charge.
CREATE TABLE video_storage_reservations (
    file_name TEXT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    media_id BIGINT NOT NULL,
    reserved_bytes BIGINT CHECK (reserved_bytes >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX video_storage_reservations_owner ON video_storage_reservations(account_id);
CREATE INDEX video_storage_reservations_unmeasured ON video_storage_reservations(media_id)
    WHERE reserved_bytes IS NULL;

INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT source_file, account_id, media_id, reserved_bytes FROM video_drafts;
-- Unknown crash-attempt sizes fail admission closed until measured under the
-- media-row fence. Do not guess an output size from its source size.
INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT o.file_name, d.account_id, d.media_id,
       CASE WHEN o.file_name = m.file_name THEN m.file_size
            WHEN o.file_name = m.small_file_name THEN m.thumbnail_file_size END
FROM video_draft_outputs o JOIN video_drafts d USING (media_id)
JOIN media_attachments m ON m.id = d.media_id;
INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT p.file_name, d.account_id, d.media_id, p.file_size
FROM video_compatibility_posters p JOIN video_drafts d USING (media_id)
ON CONFLICT (file_name) DO NOTHING;

ALTER TABLE video_draft_outputs DROP CONSTRAINT video_draft_outputs_file_name_check;
ALTER TABLE video_draft_outputs ADD CONSTRAINT video_draft_outputs_file_name_check
    CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif|poster\.jpg)$');
