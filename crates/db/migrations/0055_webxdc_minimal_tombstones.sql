-- A deletion marker must outlive the high-volume session row it replaces.
-- Signing keys live on the ordinary account row, so queued Delete deliveries
-- do not require the Webxdc session or any of its cascading child data.
ALTER TABLE webxdc_tombstones
    DROP CONSTRAINT webxdc_tombstones_session_id_fkey;

