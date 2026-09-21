-- Preserve the author's intent instead of treating every missing description
-- as decorative, and distinguish real audio description from a soundtrack
-- that already communicates the video's important visual information.
ALTER TABLE media_attachments
    ADD COLUMN decorative boolean NOT NULL DEFAULT false,
    ADD COLUMN visuals_conveyed_in_audio boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT media_visual_audio_state
        CHECK (NOT (audio_described AND visuals_conveyed_in_audio));
