-- Every range/preview request checks access by an immutable storage key.
-- Resolve that key before joining ownership/publication metadata; never scan
-- the entire attachment/segment library to authorize one file.
CREATE INDEX media_file_access ON media_attachments(file_name) WHERE file_name IS NOT NULL;
CREATE INDEX media_small_file_access ON media_attachments(small_file_name) WHERE small_file_name IS NOT NULL;
CREATE INDEX media_segment_file_access ON media_hls_segments(cache_file);
CREATE INDEX account_avatar_access ON accounts(avatar_file_name) WHERE avatar_file_name IS NOT NULL;
CREATE INDEX account_header_access ON accounts(header_file_name) WHERE header_file_name IS NOT NULL;
