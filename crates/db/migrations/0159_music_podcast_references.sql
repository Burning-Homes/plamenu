-- Native podcast episodes join existing ordered references without Audio posts.
ALTER TABLE music_collection_entries ADD COLUMN episode_id BIGINT
    REFERENCES music_podcast_episodes(id) ON DELETE CASCADE;
ALTER TABLE music_collection_entries DROP CONSTRAINT music_collection_entries_check;
ALTER TABLE music_collection_entries ADD CONSTRAINT music_collection_entry_source
    CHECK(num_nonnulls(asset_id,status_id,episode_id)=1);
CREATE INDEX music_collection_entries_episode ON music_collection_entries(episode_id)
    WHERE episode_id IS NOT NULL;
ALTER TABLE music_queue_entries ADD COLUMN episode_id BIGINT
    REFERENCES music_podcast_episodes(id) ON DELETE CASCADE;
ALTER TABLE music_queue_entries DROP CONSTRAINT music_queue_entries_check;
ALTER TABLE music_queue_entries ADD CONSTRAINT music_queue_entry_source
    CHECK(num_nonnulls(asset_id,status_id,episode_id)=1);
CREATE INDEX music_queue_entries_episode ON music_queue_entries(episode_id)
    WHERE episode_id IS NOT NULL;
ALTER TABLE music_listen_sessions ADD COLUMN episode_id BIGINT
    REFERENCES music_podcast_episodes(id) ON DELETE CASCADE;
ALTER TABLE music_listen_sessions DROP CONSTRAINT music_listen_sessions_check;
ALTER TABLE music_listen_sessions ADD CONSTRAINT music_listen_source
    CHECK(num_nonnulls(asset_id,status_id,episode_id)=1);
CREATE INDEX music_listen_sessions_episode ON music_listen_sessions(episode_id)
    WHERE episode_id IS NOT NULL;
