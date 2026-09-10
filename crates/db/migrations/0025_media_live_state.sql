-- Live-stream state for a remote video attachment (PeerTube live broadcasts).
--
-- A PeerTube live is a `Video` object that exists *before* it has any media:
-- it is created in state WAITING_FOR_LIVE with no `application/x-mpegURL`
-- entry in `url` at all, gains a master playlist (and state PUBLISHED) once
-- RTMP has been flowing for a few segments, and drops to LIVE_ENDED — or back
-- to WAITING_FOR_LIVE, when the live is permanent — when the broadcast stops.
--
-- That state is not derivable from anything we already store. The master URL
-- is a poor proxy for it: a finished live keeps advertising its master in the
-- AP object for minutes after the segments are deleted, so "the playlist
-- fetch failed" and "the stream is over" are different facts and must not be
-- conflated. Likewise a *replayed* live is indistinguishable from a live one
-- by `isLiveBroadcast` alone — only the state (plus a now-present rendition
-- ladder) separates them.
--
-- NULL means "not a live" and is the overwhelming majority of rows, so this
-- costs the ordinary attachment nothing.
ALTER TABLE media_attachments
    ADD COLUMN live_state text,
    ADD COLUMN live_permanent boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT media_attachments_live_state_check
        CHECK (live_state IS NULL OR live_state IN ('waiting', 'live', 'ended'));

-- The live-playback lanes (segment cache eviction, the progressive gateway's
-- session bookkeeping) sweep by state, and currently-live rows are a handful
-- at any moment — a partial index keeps that lookup free of the full table.
CREATE INDEX media_attachments_live_state_idx
    ON media_attachments (live_state) WHERE live_state IS NOT NULL;
