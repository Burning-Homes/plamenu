-- A ready, unpublished Video may receive a creator-uploaded poster. Revision
-- keys are immutable and never overwrite a generated or earlier uploaded
-- poster; retired keys remain charged until cleanup acknowledges deletion.
ALTER TABLE video_drafts
    ADD COLUMN poster_alt TEXT
        CHECK (poster_alt IS NULL OR char_length(poster_alt) BETWEEN 1 AND 500);

ALTER TABLE video_draft_outputs DROP CONSTRAINT video_draft_outputs_file_name_check;
ALTER TABLE video_draft_outputs ADD CONSTRAINT video_draft_outputs_file_name_check
    CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif|poster\.jpg|hls[0-9]+\.m4s|poster[0-9]+\.(avif|jpg))$');
