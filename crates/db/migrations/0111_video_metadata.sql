-- Metadata follows the one primary Video media row through private draft,
-- scheduled publication and remote Video updates. No inferred reuse licence.
CREATE TABLE video_metadata (
    media_id bigint PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    category integer CHECK (category BETWEEN 1 AND 18),
    licence integer CHECK (licence BETWEEN 1 AND 9),
    spoiler_text text NOT NULL DEFAULT '' CHECK (char_length(spoiler_text) <= 500),
    sensitive boolean NOT NULL DEFAULT false
);
