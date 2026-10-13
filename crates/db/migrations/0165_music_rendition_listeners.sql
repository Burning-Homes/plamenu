ALTER TABLE music_renditions ADD COLUMN keep_running BOOLEAN NOT NULL DEFAULT true;
CREATE TABLE music_rendition_listeners (
    id BIGINT PRIMARY KEY,
    rendition_id BIGINT NOT NULL REFERENCES music_renditions(id) ON DELETE CASCADE,
    lease_until TIMESTAMPTZ NOT NULL DEFAULT now()+interval '45 seconds'
);
CREATE INDEX music_rendition_listener_lease ON music_rendition_listeners(rendition_id,lease_until);
