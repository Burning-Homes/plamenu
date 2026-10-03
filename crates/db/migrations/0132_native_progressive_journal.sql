-- The optional MP4 remains an explicit, generation-fenced journal output.
ALTER TABLE video_draft_outputs DROP CONSTRAINT video_draft_outputs_file_name_check;
ALTER TABLE video_draft_outputs ADD CONSTRAINT video_draft_outputs_file_name_check
    CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif|poster\.jpg|hls[0-9]+\.m4s|poster[0-9]+\.(avif|jpg)|storyboard\.jpg|progressive[0-9]+\.mp4)$');
