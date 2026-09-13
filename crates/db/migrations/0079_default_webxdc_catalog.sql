-- Make the established Webxdc app catalog available out of the box.  A
-- negative id cannot collide with runtime snowflake ids.  Preserve an
-- administrator's existing source (including its chosen name and refresh
-- history) when the feed URL was already configured before this migration.
INSERT INTO webxdc_catalog_sources (id, name, feed_url)
VALUES (-1, 'Webxdc Apps', 'https://apps.testrun.org/xdcget-lock.json')
ON CONFLICT (feed_url) DO NOTHING;
