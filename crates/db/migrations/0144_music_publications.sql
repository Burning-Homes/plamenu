-- A publication is a reference to retained music, not ownership of its bytes.
-- Withdrawing it removes this edge; republishing gets a fresh status identity.
CREATE TABLE music_publications (
    asset_id BIGINT PRIMARY KEY REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT NOT NULL UNIQUE REFERENCES statuses(id) ON DELETE CASCADE,
    media_id BIGINT NOT NULL UNIQUE REFERENCES media_attachments(id) ON DELETE CASCADE
);
