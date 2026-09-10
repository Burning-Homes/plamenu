-- The anonymous landing page — the instance's public face at `/` — and its
-- sections. All on by default; turning the master switch off restores the
-- previous redirect-to-login behaviour for signed-out visitors.
ALTER TABLE instance_settings
    ADD COLUMN landing_page boolean NOT NULL DEFAULT true,
    ADD COLUMN landing_show_stats boolean NOT NULL DEFAULT true,
    ADD COLUMN landing_show_directory boolean NOT NULL DEFAULT true,
    ADD COLUMN landing_show_groups boolean NOT NULL DEFAULT true,
    ADD COLUMN landing_show_staff boolean NOT NULL DEFAULT true;
