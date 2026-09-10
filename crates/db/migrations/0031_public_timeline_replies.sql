-- Whether the public and local timelines carry replies (FEEDS_DESIGN S1 / D8).
--
-- Until now they did, which is Pleroma's shape inside a Mastodon-shaped API:
-- Mastodon's `PublicFeed` merges `without_replies` unconditionally and nothing
-- in its codebase opts in. The result here was a shared timeline where a
-- stranger's half of a conversation reads as a top-level post.
--
-- Default `false` = Mastodon behaviour, so existing instances change: replies
-- to other people leave the public and local timelines. Self-threads stay —
-- Mastodon's scope is `not_reply OR reply_to_account`, and a thread its author
-- wrote alone is a post, not a conversation fragment. Hashtag timelines are
-- deliberately untouched (Mastodon's `TagFeed` skips both scopes) and so are
-- home and list, which have their own per-follow control.
--
-- An operator who wants the old firehose turns this on.
ALTER TABLE instance_settings
    ADD COLUMN public_timeline_replies boolean NOT NULL DEFAULT false;
