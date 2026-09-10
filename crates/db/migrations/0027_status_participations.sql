-- RSVPs to event statuses (E2/E3): the participation verb family
-- `Join` / `Accept(Join)` / `Reject(Join)` / `Leave`, plus `Invite`.
--
-- Mirrors `status_dislikes` (F4b.3): one row per (status, account), dying with
-- either side. Unlike a favourite, an RSVP is a *negotiation* — proposed, then
-- accepted or refused by the organizer — so the row carries a state rather than
-- merely existing.
--
--   invited   the organizer asked *us*, unprompted (`Invite`). Not attendance:
--             it is standing permission to join an event whose join mode would
--             otherwise refuse us (`invite`), and it is the only state the
--             attendee did not initiate.
--   pending   someone asked; nobody has answered yet. On a `restricted` event
--             this is the normal resting state, possibly for days. It is also
--             where a `Join` that hit `maximum_attendee_capacity` stops
--             **forever**: Mobilizon sends no rejection activity when an event
--             is full, so an RSVP that never resolves is an ordinary outcome,
--             not a stuck job, and nothing may retry or expire it into failure.
--   accepted  an `Accept(Join)` came back (or our own auto-accept fired).
--   rejected  a `Reject(Join)` came back. Kept, not deleted, so a refused
--             attendee is not offered the button again as if nothing had
--             happened, and so a redelivered `Join` cannot launder a refusal
--             into a fresh pending row.
--
-- `uri` is the `Join` activity's id: **ours** when we RSVP outbound, **theirs**
-- when a remote attendee joins a local event. That is what lets an inbound
-- `Accept`/`Reject` naming (or embedding) a `Join` find the row it answers — the
-- activity id is the only handle the origin echoes back. An `invited` row has
-- none until the invitee actually joins.
CREATE TABLE status_participations (
    id bigint PRIMARY KEY,
    status_id bigint NOT NULL REFERENCES statuses (id) ON DELETE CASCADE,
    account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    state text NOT NULL,
    uri text,
    -- The attendee's optional note to the organizer (`participationMessage`).
    message text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT status_participations_account_id_status_id_key
        UNIQUE (account_id, status_id),
    CONSTRAINT status_participations_state_check
        CHECK (state IN ('invited', 'pending', 'accepted', 'rejected'))
);

-- The organizer's attendee list, and the accepted-count of a local event, both
-- read every row of one status.
CREATE INDEX idx_status_participations_status
    ON status_participations (status_id);

-- An inbound `Accept`/`Reject` arrives naming the `Join` activity and nothing
-- else, so the uri is a lookup key in its own right. Partial: an `invited` row
-- has no uri, and a locally auto-accepted RSVP on a local event never federates.
CREATE INDEX idx_status_participations_uri
    ON status_participations (uri) WHERE uri IS NOT NULL;
