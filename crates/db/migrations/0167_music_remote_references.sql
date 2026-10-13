-- Remote references remain origin-controlled; membership never grants access.
ALTER TABLE music_collection_entries ADD COLUMN remote_id BIGINT REFERENCES music_remote_resources(id) ON DELETE CASCADE;
ALTER TABLE music_collection_entries DROP CONSTRAINT music_collection_entry_source;
ALTER TABLE music_collection_entries ADD CONSTRAINT music_collection_entry_source CHECK(num_nonnulls(asset_id,status_id,episode_id,remote_id)=1);
CREATE INDEX music_collection_entries_remote ON music_collection_entries(remote_id) WHERE remote_id IS NOT NULL;
ALTER TABLE music_queue_entries ADD COLUMN remote_id BIGINT REFERENCES music_remote_resources(id) ON DELETE CASCADE;
ALTER TABLE music_queue_entries DROP CONSTRAINT music_queue_entry_source;
ALTER TABLE music_queue_entries ADD CONSTRAINT music_queue_entry_source CHECK(num_nonnulls(asset_id,status_id,episode_id,remote_id)=1);
CREATE INDEX music_queue_entries_remote ON music_queue_entries(remote_id) WHERE remote_id IS NOT NULL;
ALTER TABLE music_favourites ADD COLUMN remote_id BIGINT REFERENCES music_remote_resources(id) ON DELETE CASCADE;
ALTER TABLE music_favourites DROP CONSTRAINT music_favourites_check;
ALTER TABLE music_favourites ADD CONSTRAINT music_favourite_source CHECK(num_nonnulls(asset_id,status_id,remote_id)=1);
CREATE UNIQUE INDEX music_favourites_remote ON music_favourites(account_id,remote_id) WHERE remote_id IS NOT NULL;
ALTER TABLE music_listen_sessions ADD COLUMN remote_id BIGINT REFERENCES music_remote_resources(id) ON DELETE CASCADE;
ALTER TABLE music_listen_sessions DROP CONSTRAINT music_listen_source;
ALTER TABLE music_listen_sessions ADD CONSTRAINT music_listen_source CHECK(num_nonnulls(asset_id,status_id,episode_id,remote_id)=1);
CREATE INDEX music_listen_sessions_remote ON music_listen_sessions(remote_id) WHERE remote_id IS NOT NULL;
