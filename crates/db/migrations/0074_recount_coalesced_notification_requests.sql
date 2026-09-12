-- Migration 73 can collapse several historical filtered mention/quote rows
-- into one canonical notification. Repair the request rollup after that
-- deduplication without rewriting the already-applied migration.
UPDATE notification_requests r
SET notifications_count = LEAST(100, (
    SELECT count(*)
    FROM notifications n
    WHERE n.account_id = r.account_id
      AND n.from_account_id = r.from_account_id
      AND n.filtered
      AND n.reasons && ARRAY['mention', 'quote']::TEXT[]
));
