-- Titled posts (Lemmy-style Pages, F4b group submissions) often carry their
-- whole meaning in `statuses.title`, which the translation pipeline ignored —
-- a title-only Page translated to nothing. The cached translation now stores
-- the translated title alongside the body.
ALTER TABLE status_translations
    ADD COLUMN title TEXT NOT NULL DEFAULT '';
