-- The character limit a long-form post is measured against.
--
-- `max_characters` (default 500, Mastodon's) is the cap for an ordinary post and
-- stays exactly that. An `Article` is a different kind with a different purpose:
-- under a 500-character cap "long-form" is a contradiction, and raising the
-- global limit to accommodate one kind changes every post on the instance.
--
-- 50 000 is deliberately generous — roughly a 8-10k-word essay — because the
-- real ceilings are elsewhere (delivery payload size, what a peer will accept),
-- and an operator who wants a tighter editorial limit can set one. The column is
-- NOT NULL with a default so existing instances get the limit without an
-- operator touching the admin console.
ALTER TABLE instance_settings
    ADD COLUMN max_characters_long_form integer NOT NULL DEFAULT 50000;
