-- The rest of an `Event` object's facts (E1).
--
-- F3 stored the four fields a calendar box needs to render at all — when,
-- where (as a flattened name), which zone, whether it still happens. Everything
-- else Mobilizon sends was parsed and dropped, which made an event card inert:
-- without `join_mode` there is no way to know whether an RSVP button is even
-- meaningful, without `external_participation_url` an `external` event has
-- nowhere to send the viewer, and without the capacity pair a full event looks
-- joinable.
--
-- All nullable. Mobilizon is the richest dialect by a wide margin; Gancio and
-- Friendica send a small subset, and an `Event` from a general-purpose server
-- may carry nothing but `name` + `startTime`. A missing column therefore means
-- "the origin didn't say", never "false" — which is why `is_online`,
-- `comments_enabled` and `anonymous_participation` are nullable booleans rather
-- than NOT NULL DEFAULT false.
ALTER TABLE status_events
    -- `free` | `restricted` | `invite` | `external` (Mobilizon's joinModeType,
    -- lowercased). Gates the RSVP affordance: `invite` shows no button at all,
    -- `external` links out to `external_participation_url` instead of
    -- generating a Join.
    ADD COLUMN join_mode text,
    -- The origin's own attendee count. For a REMOTE event this is the only
    -- honest count: we are addressed on a subset of the participation
    -- activities, so counting our own sidecar rows would systematically
    -- undercount. Local events count from `status_participations` instead —
    -- the two are never mixed.
    ADD COLUMN participant_count integer,
    ADD COLUMN max_attendees integer,
    ADD COLUMN remaining_attendees integer,
    -- Where to RSVP when `join_mode = 'external'` (ticketing, another
    -- platform). Absent otherwise.
    ADD COLUMN external_participation_url text,
    ADD COLUMN anonymous_participation boolean,
    ADD COLUMN is_online boolean,
    -- Mobilizon's own category vocabulary (`MEETING`, `SPORTS`, …), kept as
    -- sent: it is not an interoperable enum, so it is a display hint only and
    -- must not gate behaviour.
    ADD COLUMN comments_enabled boolean,
    ADD COLUMN category text,
    -- The structured `Place`. F3 flattened the whole thing to `location_name`,
    -- which throws away a usable address and the Place's own id — the latter
    -- being the only stable handle on a venue that several events share.
    ADD COLUMN location_url text,
    ADD COLUMN location_street text,
    ADD COLUMN location_locality text,
    ADD COLUMN location_region text,
    ADD COLUMN location_country text,
    ADD COLUMN location_postal_code text,
    ADD CONSTRAINT status_events_join_mode_check
        CHECK (join_mode IS NULL
               OR join_mode IN ('free', 'restricted', 'invite', 'external'));
