CREATE INDEX discovery_page_receipts_expiry ON discovery_page_receipts(created_at);
-- Reuse already imported, canonical language evidence without remote requests.
UPDATE discovery_entries e SET language=s.language
FROM statuses s WHERE s.uri=e.canonical_uri AND e.kind IN ('post','video')
 AND e.state='active' AND e.language IS NULL AND s.language IS NOT NULL;
