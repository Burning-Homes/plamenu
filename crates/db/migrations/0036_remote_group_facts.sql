-- What a remote community tells us about itself, mirrored locally.
--
-- A Group actor carries three community facts beyond the shared profile:
-- Lemmy's `sensitive` (the community-wide NSFW flag), its posting
-- restriction, and the FEP-1b12 `attributedTo` moderators collection. We
-- publish all three for the groups we host and, until now, read none of them
-- back: a consumed community's NSFW flag was dropped, its mods-only policy
-- was invisible (so the client offered a "New post" button the origin would
-- reject), and its moderator list was learned only from live `Add`/`Remove`
-- activities — never by dereferencing the collection the way Lemmy does at
-- ingest.
--
-- These live in their own table rather than in `groups`, which is
-- deliberately the sidecar of a *hosted* group: `group::find` returning
-- `Some` is the "we own this group's policy" test in the posting gate, the
-- moderation console and the actor serializer. Mirrored remote facts are not
-- policy we enforce — they are the origin's claims, refreshed on every actor
-- refresh and never authoritative for anything we sign.
CREATE TABLE remote_groups (
    account_id bigint PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    -- Lemmy's community-wide NSFW flag (`as:sensitive` on the actor).
    sensitive boolean NOT NULL DEFAULT false,
    -- Who may start threads, resolved to the same tri-state as a hosted
    -- group's `groups.posting_policy`. Lemmy's `postingRestrictedToMods` can
    -- only say mods-or-not, so a peer running this software publishes the full
    -- policy under our own `postingPolicy` term and that wins when present;
    -- everyone else resolves to 'mods' or 'anyone' from the boolean. Stored
    -- resolved rather than raw so the posting gate reads one column and local
    -- and remote groups answer the same question the same way.
    posting_policy text NOT NULL DEFAULT 'anyone',
    -- The `attributedTo` moderators collection, or the FEP-5219 `affiliations`
    -- collection when that is all the actor publishes (Mitra). Empty when the
    -- actor advertises neither, or lists its moderators inline (PeerTube).
    moderators_uri text NOT NULL DEFAULT '',
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The mirrored moderator roster of a remote community.
--
-- Kept apart from `group_affiliations` for the same reason as above: that
-- table is the FEP-5219 ladder we enforce and federate for hosted groups
-- (owner/moderator/outcast, with `Add`/`Remove` fan-out on every write), and
-- a remote roster must never be mistaken for one — being listed as a mod of
-- someone else's community grants nothing here.
--
-- `ordinal` preserves the collection's own order (Lemmy puts the community
-- creator first), so the roster renders the way the origin lists it.
CREATE TABLE remote_group_moderators (
    group_account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    ordinal integer NOT NULL,
    PRIMARY KEY (group_account_id, account_id)
);

-- The moderator side of the pair, so deleting an account that moderates
-- remote communities does not scan the table (the rule stated in 0034); the
-- group side rides the primary key.
CREATE INDEX idx_remote_group_moderators_account
    ON remote_group_moderators (account_id);
