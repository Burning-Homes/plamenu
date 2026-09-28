-- Scheduled Videos use the existing leased scheduled-status queue, but retain
-- their publishing channel as first-class data for the due transaction.
ALTER TABLE scheduled_statuses
    ADD COLUMN video_channel_id BIGINT REFERENCES video_channels(account_id) ON DELETE RESTRICT;
ALTER TABLE scheduled_statuses
    ADD CONSTRAINT scheduled_video_shape CHECK (
        (object_type = 'Video' AND video_channel_id IS NOT NULL
         AND cardinality(media_ids) = 1)
        OR (object_type <> 'Video' AND video_channel_id IS NULL)
    );
