-- Default-on by product decision; durable HTML-first opt-out per local account.
ALTER TABLE video_viewer_preferences ADD COLUMN p2p_enabled boolean NOT NULL DEFAULT true;
