-- O3: record when the hourly maintenance worker auto-reverted open
-- registration to approval mode (no moderator active for a week), so the
-- admin console can explain the flip instead of it happening silently.
-- Cleared whenever an operator saves the admin settings form.
ALTER TABLE instance_settings
    ADD COLUMN registrations_auto_closed_at timestamptz;
