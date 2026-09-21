-- Structured alternatives for locally-authored time-based media.
-- Remote/federated rows leave these NULL/false unless a future interoperable
-- ActivityPub extension supplies equivalent data.
ALTER TABLE media_attachments
    ADD COLUMN transcript text,
    ADD COLUMN caption_vtt text,
    ADD COLUMN audio_described boolean NOT NULL DEFAULT false;
