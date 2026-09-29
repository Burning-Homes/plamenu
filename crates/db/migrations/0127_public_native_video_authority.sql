-- One origin authority predicate for MP4, HLS, captions, replay and tracker
-- consumers. Publication signatures retain their original author after an
-- owner transfer; public delivery also requires the current owner to be active.
CREATE VIEW public_native_videos AS
SELECT v.media_id, v.status_id, v.channel_id, v.uuid
FROM video_publications v
JOIN media_attachments m ON m.id = v.media_id AND m.status_id = v.status_id
JOIN statuses s ON s.id = v.status_id
JOIN accounts author ON author.id = s.account_id
JOIN accounts uploader ON uploader.id = m.account_id
JOIN accounts channel ON channel.id = v.channel_id
JOIN video_channels vc ON vc.account_id = v.channel_id
JOIN accounts owner ON owner.id = vc.owner_account_id
WHERE s.deleted_at IS NULL AND s.visibility IN ('public', 'unlisted')
  AND author.suspended_at IS NULL AND author.deleted_at IS NULL
  AND uploader.suspended_at IS NULL AND uploader.deleted_at IS NULL
  AND channel.suspended_at IS NULL AND channel.deleted_at IS NULL
  AND owner.suspended_at IS NULL AND owner.deleted_at IS NULL;
