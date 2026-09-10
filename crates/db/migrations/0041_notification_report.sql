-- The `admin.report` staff notification embeds the report it announces
-- (Mastodon's NotificationSerializer `report`), mirroring
-- `account_warning_id` on `moderation_warning`. Rows die with the report.
ALTER TABLE notifications
    ADD COLUMN report_id BIGINT REFERENCES reports(id) ON DELETE CASCADE;
