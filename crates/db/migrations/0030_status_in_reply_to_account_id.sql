-- Who wrote the post a reply answers (`FEEDS_DESIGN.md` S0), plus the thread
-- repair that keeps the answer knowable.
--
-- Every reply-aware feed rule the feed design needs asks the same question —
-- "is this a self-thread", "is this a reply to *me*", "is this a reply to
-- someone I follow" — and each of those needs the *parent's* author. Today that
-- costs a probe back into `statuses` per candidate row (the shape the list
-- timeline uses), which is exactly the per-row pathology the performance pass
-- hoisted out of the home timeline's driving scan. On the public timeline it is
-- worse: that query walks the global `idx_statuses_sort_at` order where nearly
-- every row passes its filters, so a parent probe would land on every row of the
-- hot path. Denormalizing the answer turns three hot-path probes into column
-- comparisons.
--
-- The column is derived from `in_reply_to_id` and nothing else, so it is exactly
-- as good as the reply *edge* — which is where the second half of this migration
-- comes in: a reply whose parent had not arrived yet (`in_reply_to_uri` set,
-- `in_reply_to_id` NULL — the origin 403s our pull, a group announced a comment
-- whose root we never had, the host was down) was never linked to that parent
-- afterwards, even once it turned up by some other route. Those replies stayed
-- detached from their thread forever, and would additionally have become
-- permanently unclassifiable to the reply rules: hidden from the public
-- timeline for good, even when they were self-threads that should have shown.
-- So this migration links them, and `status::adopt_orphan_replies` keeps doing
-- it from now on at the moment a parent arrives.
--
-- No foreign key on the new column: `in_reply_to_id`'s own FK already guarantees
-- integrity, and an `accounts` reference would make every account deletion
-- sequential-scan `statuses`.
ALTER TABLE statuses ADD COLUMN in_reply_to_account_id bigint;

-- Filled in the database rather than by each writer, deliberately. There are
-- four `INSERT INTO statuses` sites, the late-linking UPDATE below, and the
-- self-FK's `ON DELETE SET NULL` action, and this repo has a standing trap where
-- a new `statuses` field added to one write path silently misses another. A
-- BEFORE trigger keyed to `in_reply_to_id` makes the invariant hold for
-- whichever path ran — including the FK action, which nulls the cache when a
-- parent is hard-deleted, and any future writer nobody thought to update.
--
-- A NULL `in_reply_to_id` makes the scalar subquery return NULL, so the same
-- function serves both directions (parent gained, parent lost).
CREATE FUNCTION public.status_reply_author() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.in_reply_to_account_id := (
        SELECT account_id FROM statuses WHERE id = NEW.in_reply_to_id
    );
    RETURN NEW;
END $$;

-- Gated on being a reply: a plain post (the overwhelming majority of inserts)
-- never enters plpgsql, and the column keeps its NULL default.
CREATE TRIGGER statuses_reply_author_insert
    BEFORE INSERT ON statuses
    FOR EACH ROW
    WHEN (NEW.in_reply_to_id IS NOT NULL)
    EXECUTE FUNCTION public.status_reply_author();

CREATE TRIGGER statuses_reply_author_update
    BEFORE UPDATE OF in_reply_to_id ON statuses
    FOR EACH ROW
    WHEN (NEW.in_reply_to_id IS DISTINCT FROM OLD.in_reply_to_id)
    EXECUTE FUNCTION public.status_reply_author();

-- The reply chain above a status, itself included — the cycle guard for late
-- linking. Every reply edge stored so far points from a newer row to an older
-- one, because a reply can only resolve a parent that already exists; linking a
-- reply to a parent that arrived *later* is the first edge that runs the other
-- way, so "is this candidate parent already below the reply?" stops being
-- impossible by construction and has to be asked. A peer publishing two posts
-- that each claim to answer the other is all it takes.
--
-- The walk is the *whole* chain, not a bounded prefix: a loop can be any length
-- (A answers B answers C answers A), and a depth-capped guard would happily
-- close one longer than its cap while a fail-closed cap would refuse adoption in
-- genuinely deep threads. `CYCLE` is what makes an uncapped walk safe — a
-- repeated id is emitted once and never expanded — so the function terminates
-- even on a table that somehow already contains a loop, which is also why it
-- does not simply reuse the display-oriented `THREAD_DEPTH_LIMIT`.
CREATE FUNCTION public.status_ancestor_ids(start bigint) RETURNS SETOF bigint
    LANGUAGE sql STABLE
    AS $$
    WITH RECURSIVE chain(id, parent) AS (
        SELECT s.id, s.in_reply_to_id FROM statuses s WHERE s.id = start
        UNION ALL
        SELECT s.id, s.in_reply_to_id
        FROM statuses s JOIN chain c ON s.id = c.parent
    ) CYCLE id SET looped USING path
    SELECT id FROM chain
$$;

-- Backfill, in bounded batches rather than one table-wide UPDATE: on a busy
-- instance the single-statement form holds row locks over the whole reply
-- population at once and writes one enormous WAL record.
DO $$
DECLARE
    touched integer;
BEGIN
    -- Pass 1: replies already linked to a parent row — read the author off it.
    LOOP
        UPDATE statuses s
        SET in_reply_to_account_id = p.account_id
        FROM statuses p
        WHERE p.id = s.in_reply_to_id
          AND s.id IN (
              SELECT id FROM statuses
              WHERE in_reply_to_id IS NOT NULL
                AND in_reply_to_account_id IS NULL
              LIMIT 5000
          );
        GET DIAGNOSTICS touched = ROW_COUNT;
        EXIT WHEN touched = 0;
    END LOOP;
    -- Pass 2: replies that were ingested before their parent, whose parent is
    -- sitting in the table unlinked. Setting the edge fires the trigger above,
    -- so the author cache follows for free. The conversation these replies
    -- minted for themselves is left alone here — flat/context view already
    -- coheres by `context` IRI for the peers that send one, and the runtime path
    -- (`conversation::absorb_placeholder`) handles the rest as parents arrive.
    --
    -- The batch is drawn from the *linkable* rows, not from all orphans: most
    -- orphans have no parent here at all, and a batch of those would report zero
    -- rows touched and stop the loop before reaching the ones that do.
    LOOP
        UPDATE statuses s
        SET in_reply_to_id = adoptable.parent_id
        FROM (
            SELECT s2.id AS reply_id, p.id AS parent_id
            FROM statuses s2
            JOIN statuses p ON p.uri = s2.in_reply_to_uri
            WHERE s2.in_reply_to_id IS NULL
              AND s2.in_reply_to_uri IS NOT NULL
              AND p.id <> s2.id
              AND NOT EXISTS (
                  SELECT 1 FROM public.status_ancestor_ids(p.id) a WHERE a = s2.id
              )
            LIMIT 5000
        ) adoptable
        WHERE s.id = adoptable.reply_id;
        GET DIAGNOSTICS touched = ROW_COUNT;
        EXIT WHEN touched = 0;
    END LOOP;
END $$;

-- The lookup every inbound remote status now performs: "does anything already
-- stored reply to *this* URI without having found it?". Partial on the orphan
-- set, a small minority of rows, so the probe stays an index hit on a tiny index
-- and adds no write amplification to the live set. It also shrinks as replies
-- are adopted — a row leaves the index the moment it gains its parent.
CREATE INDEX idx_statuses_orphan_reply_uri
    ON statuses (in_reply_to_uri)
    WHERE in_reply_to_id IS NULL AND in_reply_to_uri IS NOT NULL;
