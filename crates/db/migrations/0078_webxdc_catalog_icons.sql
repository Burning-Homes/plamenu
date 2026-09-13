-- Catalog icon locations are advisory discovery metadata.  Bytes are fetched
-- lazily through the guarded federation client and cached with the candidate,
-- so catalog browsing neither contacts remote hosts from the browser nor
-- downloads every icon during a manual feed refresh.
ALTER TABLE webxdc_catalog_candidates
    ADD COLUMN icon_url text,
    ADD COLUMN icon_media_type text,
    ADD COLUMN icon_bytes bytea,
    ADD CONSTRAINT webxdc_catalog_candidate_icon_cache
        CHECK ((icon_media_type IS NULL) = (icon_bytes IS NULL)),
    ADD CONSTRAINT webxdc_catalog_candidate_icon_size
        CHECK (icon_bytes IS NULL OR octet_length(icon_bytes) <= 524288);
