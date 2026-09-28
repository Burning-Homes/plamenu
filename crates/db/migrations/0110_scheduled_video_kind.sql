-- The original queue shape admitted Notes and Articles only. Video keeps a
-- required title and channel, and cannot be a reply or a poll.
ALTER TABLE scheduled_statuses
    DROP CONSTRAINT scheduled_statuses_kind_check,
    ADD CONSTRAINT scheduled_statuses_kind_check CHECK (
        (object_type = 'Note' AND title IS NULL)
        OR (object_type = 'Article' AND title IS NOT NULL
            AND char_length(btrim(title)) BETWEEN 1 AND 200
            AND in_reply_to_id IS NULL AND poll_options IS NULL)
        OR (object_type = 'Video' AND title IS NOT NULL
            AND char_length(btrim(title)) BETWEEN 3 AND 120
            AND in_reply_to_id IS NULL AND poll_options IS NULL)
    );
