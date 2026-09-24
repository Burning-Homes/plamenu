-- Language-specific caption tracks. The legacy media_attachments.caption_vtt
-- remains readable for existing local uploads; this sidecar supports remote
-- PeerTube tracks now and local multi-track authoring later.
CREATE TABLE video_captions (
    media_id bigint NOT NULL REFERENCES media_attachments(id) ON DELETE CASCADE,
    language varchar(35) NOT NULL,
    vtt_body text,
    remote_url text,
    automatically_generated boolean NOT NULL DEFAULT false,
    PRIMARY KEY (media_id, language),
    CONSTRAINT video_captions_one_source CHECK ((vtt_body IS NULL) <> (remote_url IS NULL)),
    CONSTRAINT video_captions_language CHECK (language ~ '^[A-Za-z]{2,8}(-[A-Za-z0-9]{1,8})*$'),
    CONSTRAINT video_captions_body_limit CHECK (vtt_body IS NULL OR octet_length(vtt_body) <= 1048576),
    CONSTRAINT video_captions_url_limit CHECK (remote_url IS NULL OR length(remote_url) <= 2048)
);
