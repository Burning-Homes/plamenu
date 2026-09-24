-- One separately published replay per reusable broadcast session. The media
-- link is allocated before Create(Video); a crashed worker can reuse it.
CREATE UNIQUE INDEX native_live_sessions_replay_media_unique
    ON native_live_sessions(replay_media_id) WHERE replay_media_id IS NOT NULL;
