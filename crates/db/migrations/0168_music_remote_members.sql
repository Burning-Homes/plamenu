-- Collection membership belongs to fetched pages, not to an object's metadata parent.
CREATE TABLE music_remote_members (
    parent_uri TEXT NOT NULL REFERENCES music_remote_resources(uri) ON DELETE CASCADE,
    page_uri TEXT NOT NULL CHECK(length(page_uri)<=2048),
    reference_uri TEXT NOT NULL CHECK(length(reference_uri)<=2048),
    resource_uri TEXT NOT NULL REFERENCES music_remote_resources(uri) ON DELETE CASCADE,
    source_uri TEXT REFERENCES music_remote_resources(uri) ON DELETE CASCADE,
    fetched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY(parent_uri,page_uri,reference_uri)
);
CREATE INDEX music_remote_members_resource ON music_remote_members(resource_uri,parent_uri);
CREATE INDEX music_remote_members_source ON music_remote_members(source_uri) WHERE source_uri IS NOT NULL;
