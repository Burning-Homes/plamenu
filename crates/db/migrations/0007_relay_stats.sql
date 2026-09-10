-- Per-relay inbound volume: one row per relay per UTC day, upsert-incremented
-- at the relay-Announce choke point in the inbox (the daily_interactions
-- pattern, keyed per relay). Powers the admin relay cards' all-time and
-- last-7-days counters. A deleted relay takes its history with it.
CREATE TABLE relay_daily_activities (
    relay_id bigint NOT NULL REFERENCES relays(id) ON DELETE CASCADE,
    day date NOT NULL,
    count bigint DEFAULT 0 NOT NULL,
    PRIMARY KEY (relay_id, day)
);
