-- Webxdc coordinators are implementation actors, not ordinary Plamenu
-- accounts or Lemmy-style communities. Keep the account row while queued
-- federation deliveries still need its signing keys, but make that purpose
-- explicit so generic discovery never exposes it as a normal Group.
ALTER TABLE accounts
    ADD COLUMN is_internal boolean NOT NULL DEFAULT false;

ALTER TABLE webxdc_tombstones
    ADD COLUMN coordinator_account_id bigint,
    ADD COLUMN coordinator_username text;

UPDATE webxdc_tombstones t
SET coordinator_account_id = a.id,
    coordinator_username = CASE WHEN a.domain IS NULL THEN a.username END
FROM accounts a
WHERE a.uri = t.coordinator_uri;

UPDATE accounts a
SET is_internal = true
WHERE EXISTS (
        SELECT 1 FROM webxdc_sessions s WHERE s.account_id = a.id
    )
   OR EXISTS (
        SELECT 1 FROM webxdc_tombstones t
        WHERE t.coordinator_account_id = a.id
    );

CREATE INDEX idx_accounts_internal
    ON accounts (id) WHERE is_internal;

CREATE INDEX idx_webxdc_tombstones_coordinator_cleanup
    ON webxdc_tombstones (deleted_at, coordinator_account_id)
    WHERE coordinator_account_id IS NOT NULL;

CREATE UNIQUE INDEX idx_webxdc_tombstones_coordinator_username
    ON webxdc_tombstones (lower(coordinator_username))
    WHERE coordinator_username IS NOT NULL;
