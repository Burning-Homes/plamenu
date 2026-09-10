-- The local-groups sample left the landing page too (it was the last inlined
-- list; the welcome page is hero + stats + description + footer now), so its
-- switch has nothing left to gate. IF EXISTS for dev databases altered by
-- hand while iterating.
ALTER TABLE instance_settings
    DROP COLUMN IF EXISTS landing_show_groups;
