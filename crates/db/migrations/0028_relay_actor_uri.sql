-- Identify a relay by its ACTOR, not only by an inbox URL.
--
-- A relay used to be recognised by matching an inbound sender's inbox (or shared
-- inbox) against `relays.inbox_url`, following Mastodon's
-- `Relay.find_by(inbox_url:)`. That holds for dedicated relay software, whose
-- actor has an inbox of its own — and breaks badly for a *general-purpose server*
-- acting as a relay, because there the relay actor advertises the instance's
-- **shared inbox**.
--
-- Mobilizon is exactly that case: its relay actor lives at `/relay` but its inbox
-- is `/inbox`, the same shared inbox every group and person on that host
-- advertises. Subscribing to it therefore made *every actor on the host* look
-- like the relay, and a relay's `Announce` is deliberately treated as a delivery
-- hint rather than a boost — so following a Mobilizon instance silently stopped
-- its groups' events from ever appearing on a follower's timeline. The
-- subscription broke the ordinary follow it was supposed to complement.
--
-- The actor URI is the only identity that cannot collide. Recorded when the
-- subscription resolves the actor; NULL for rows created before this migration,
-- which fall back to matching the sender's *own* inbox (never the shared one).
ALTER TABLE relays ADD COLUMN actor_uri text;

CREATE UNIQUE INDEX relays_actor_uri_key ON relays (actor_uri)
    WHERE actor_uri IS NOT NULL;
