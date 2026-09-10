-- `moderation_warning` notifications (Mastodon 4.3): point the notification at
-- the strike it announces so the serializer can embed the warning (action,
-- text, appeal state). Deleting a strike takes its notification with it.
ALTER TABLE notifications ADD COLUMN account_warning_id bigint
    REFERENCES account_warnings(id) ON DELETE CASCADE;

-- Supports the FK cascade (and any strike -> notification lookup) without
-- scanning the whole notifications table; almost every row is NULL, so the
-- partial index stays tiny.
CREATE INDEX idx_notifications_account_warning ON notifications (account_warning_id)
    WHERE account_warning_id IS NOT NULL;
