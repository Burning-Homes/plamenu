-- Storyboards participate in the same generation-fenced cleanup journal as
-- MP4/HLS/posters. Keep the key allowlist explicit, not arbitrary .draft files.
ALTER TABLE video_draft_outputs DROP CONSTRAINT video_draft_outputs_file_name_check;
ALTER TABLE video_draft_outputs ADD CONSTRAINT video_draft_outputs_file_name_check
    CHECK (file_name ~ '^[a-zA-Z0-9]+\.draft\.(mp4|small\.avif|poster\.jpg|hls[0-9]+\.m4s|poster[0-9]+\.(avif|jpg)|storyboard\.jpg)$');
