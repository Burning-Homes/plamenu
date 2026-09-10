-- Coordinator-local, session-scoped participants for visitors whose home
-- server does not implement FEP-752d.  The bearer token is stored only as a
-- hash; guest identities are intentionally not ordinary Plamenu accounts and
-- cannot be used outside their one Webxdc session.
CREATE TABLE webxdc_guests (
    id bigint PRIMARY KEY,
    session_id bigint NOT NULL REFERENCES webxdc_sessions (id) ON DELETE CASCADE,
    display_name text NOT NULL,
    token_hash text NOT NULL UNIQUE,
    participant_uri text NOT NULL UNIQUE,
    accepted boolean NOT NULL DEFAULT false,
    replay_boundary bigint NOT NULL DEFAULT 0,
    self_addr text NOT NULL,
    joined_at timestamptz NOT NULL DEFAULT now(),
    accepted_at timestamptz,
    last_submitted_at timestamptz,
    CHECK (char_length(display_name) BETWEEN 1 AND 80),
    CHECK (replay_boundary >= 0 AND replay_boundary <= 9007199254740991)
);

CREATE INDEX idx_webxdc_guests_session
    ON webxdc_guests (session_id, accepted, joined_at);
