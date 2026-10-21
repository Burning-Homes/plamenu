-- Earlier quote ingestion fetched cross-origin targets only with an approval
-- stamp. Restore retries for unresolved remote quotes, including those whose
-- old retry budget expired, now that fetching is independent of approval.
-- Negative, deterministic IDs reserve a migration-only lane; runtime job IDs
-- are positive snowflakes. Existing jobs retain their schedule and lease.
INSERT INTO quote_verify_jobs (id, quote_id, run_at)
SELECT -q.id, q.id, now()
FROM quotes q
JOIN accounts a ON a.id = q.account_id
JOIN statuses s ON s.id = q.status_id
WHERE q.state = 'pending'
  AND q.quoted_status_id IS NULL
  AND q.quoted_uri IS NOT NULL
  AND a.domain IS NOT NULL
  AND s.deleted_at IS NULL
ON CONFLICT (quote_id) DO NOTHING;
