-- Distinguish the default from an explicitly chosen account preference. An
-- existing browser-only opt-out must not become opt-in merely after upgrade.
ALTER TABLE video_viewer_preferences ADD COLUMN p2p_configured boolean NOT NULL DEFAULT false;
