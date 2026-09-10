-- Applications created from the web "Development" settings page belong to the
-- user who made them (Mastodon: Doorkeeper application owner). API-registered
-- apps (`POST /api/v1/apps`) and the first-party web app stay ownerless.
ALTER TABLE oauth_apps
    ADD COLUMN owner_user_id BIGINT REFERENCES users(id) ON DELETE CASCADE;

CREATE INDEX oauth_apps_owner_user_id_idx
    ON oauth_apps (owner_user_id)
    WHERE owner_user_id IS NOT NULL;
