-- The landing-page rework moved discoverable profiles and the staff/contact
-- card off the welcome page onto always-public dedicated pages (/explore/people
-- and /staff), so their landing-section switches have nothing left to gate.
-- IF EXISTS keeps this replayable on dev databases where the columns were
-- already dropped by hand while iterating.
ALTER TABLE instance_settings
    DROP COLUMN IF EXISTS landing_show_directory,
    DROP COLUMN IF EXISTS landing_show_staff;
