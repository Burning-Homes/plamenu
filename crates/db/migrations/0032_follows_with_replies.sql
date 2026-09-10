-- Per-follow "show me their replies" (`FEEDS_DESIGN.md` S2 / D7, D13).
--
-- Home and the list timelines carry every reply from every followee today,
-- which is Pleroma's shape; Mastodon, Sharkey and GoToSocial all admit a reply
-- into a follow-feed only when the viewer has some relationship to the
-- conversation. This column is the per-relationship switch, and the feed
-- queries fall back to the three exemptions every one of those servers agrees
-- on (self-thread, reply to me, reply to someone I follow) when it is off.
--
-- Default TRUE, so no existing feed changes shape on deploy — except where the
-- default is overridden below.
ALTER TABLE follows ADD COLUMN with_replies boolean NOT NULL DEFAULT true;

-- The default depends on *who* was followed (D7, amended after the staging
-- measurement): a community and a bot arrive with replies off, a person with
-- them on. `actor_type` NULL reads as Person — an unrecognised remote `type` is
-- stored as NULL and that is 7 of the 46 follows on staging, so "unknown" must
-- not mean "filtered".
--
-- Set in the database rather than in `follow::create`, for the same reason S0's
-- `status_reply_author()` is a trigger: several paths create a follow row (a
-- local follow, an accepted inbound request, an outgoing pending follow, CSV
-- import, `bulk_import`), and this repo has a standing trap where a new column
-- added to one write path silently misses another.
--
-- The trigger assigns unconditionally on INSERT: no writer supplies the column
-- at insert time (`follow::insert` names five columns, and the only other
-- writer is `follow::update_settings`, an UPDATE), so there is nothing to
-- distinguish "left at the default" from "asked for". A future insert path that
-- wants to choose the value must set it in a follow-up UPDATE.
--
-- `follow::insert` is an upsert; on the conflicting path the computed NEW row is
-- discarded and its `DO UPDATE SET` touches only `uri`/`pending`, so a repeated
-- Follow never resets a value the user has since changed.
CREATE FUNCTION public.follow_default_with_replies() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.with_replies := NOT EXISTS (
        SELECT 1 FROM accounts a
        WHERE a.id = NEW.target_account_id
          AND (a.actor_type = 'Group' OR a.is_bot)
    );
    RETURN NEW;
END $$;

CREATE TRIGGER follows_default_with_replies
    BEFORE INSERT ON follows
    FOR EACH ROW
    EXECUTE FUNCTION public.follow_default_with_replies();

-- Backfill, or the amended default would apply only to follows made after this
-- deploy and the two populations would diverge silently: a community followed
-- yesterday would keep announcing every comment while one followed tomorrow
-- would not. The rule is an OR over both columns, so an account that is a Group
-- *and* a bot is covered once.
UPDATE follows f
SET with_replies = false
FROM accounts a
WHERE a.id = f.target_account_id
  AND (a.actor_type = 'Group' OR a.is_bot);

-- The `following_accounts.csv` column that carries the flag across an
-- export/import round trip. Nullable like its `show_reblogs` neighbour: absent
-- from the file means "no opinion", and the import leaves whatever the trigger
-- chose rather than forcing a literal.
ALTER TABLE bulk_import_rows ADD COLUMN with_replies boolean;
