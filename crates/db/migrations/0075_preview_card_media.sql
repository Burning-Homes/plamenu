-- Media discovered from link-preview metadata is a first-class attachment for
-- API clients and the built-in player, while retaining its derived provenance
-- so body edits can remove and recrawl it safely.
ALTER TABLE media_attachments
    ADD COLUMN preview_card_id bigint
        REFERENCES preview_cards(id) ON DELETE CASCADE;

CREATE INDEX media_attachments_preview_card_id_idx
    ON media_attachments (preview_card_id)
    WHERE preview_card_id IS NOT NULL;
