ALTER TABLE scheduled_statuses
    ADD COLUMN object_type text NOT NULL DEFAULT 'Note',
    ADD COLUMN title text,
    ADD CONSTRAINT scheduled_statuses_kind_check CHECK (
        (object_type = 'Note' AND title IS NULL)
        OR (object_type = 'Article' AND title IS NOT NULL
            AND char_length(btrim(title)) BETWEEN 1 AND 200
            AND in_reply_to_id IS NULL AND poll_options IS NULL)
    );
