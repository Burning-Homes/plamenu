-- Boost collapse in the built-in web client (FEEDS_DESIGN S3 / D1-D5).
--
-- Three boosts of the same post render as three cards today. Collapsing them
-- into one card carrying "Alice, Bob and 3 others boosted" is presentation
-- only: it happens after the timeline query returns, and never touches
-- /api/v1/timelines/*, so third-party clients keep the firehose and their own
-- dedup (Phanpy already ships one).
--
-- `boost_collapse` is the operator's master switch, on by default.
--
-- `boost_collapse_lookback` is the window, in posts, above the page being
-- rendered: 0 (the default) collapses within the fetched page only and costs
-- nothing, while a positive depth buys cross-page suppression at the price of
-- one extra timeline query per page — the most expensive query this server
-- runs. Mastodon's equivalent constant is REBLOG_FALLOFF = 80, but it spends
-- that at *write* time while fanning out, where we would spend it per read.
-- Capped at 500 so a typo cannot turn every feed render into a full-history
-- scan.
ALTER TABLE instance_settings
    ADD COLUMN boost_collapse boolean NOT NULL DEFAULT true,
    ADD COLUMN boost_collapse_lookback integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT instance_settings_boost_collapse_lookback_check
        CHECK (boost_collapse_lookback BETWEEN 0 AND 500);

-- The reader's own escape hatch, Mastodon's `aggregate_reblogs` (default true,
-- a user setting there too). Off restores the firehose for that reader alone,
-- whatever the operator set.
ALTER TABLE users
    ADD COLUMN reading_collapse_boosts boolean NOT NULL DEFAULT true;
