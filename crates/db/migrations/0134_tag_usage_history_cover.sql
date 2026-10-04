-- Tag histories aggregate uses and account counts over a bounded day range.
-- The primary key supplies the range, but reading uses from the heap otherwise
-- costs one heap visit per account/day. Cover the aggregate for index-only
-- reads when vacuum has made those pages visible. Keep the primary key for
-- per-account uniqueness and the account index for cascaded deletion.
-- Updating uses also maintains this index; it trades extra write/storage cost
-- for fewer heap reads on public tag and trending-tag pages.
CREATE INDEX idx_tag_usages_history_cover
    ON tag_usages (tag_id, day) INCLUDE (uses);
