-- Custom-emoji uploads use their own, much smaller budget than ordinary
-- images. Keep it beside the personal-collection quota: both are
-- installation-wide policy exposed in the admin settings page.
ALTER TABLE custom_emoji_settings
    ADD COLUMN max_file_size_kb integer NOT NULL DEFAULT 256
        CHECK (max_file_size_kb BETWEEN 1 AND 16384);
