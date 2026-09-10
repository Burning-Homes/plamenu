-- Federated Webxdc sessions (FEP-752d).
--
-- A session has a real Group account so the ordinary delivery worker can sign
-- as the coordinator.  The executable bundle and its expanded files live in
-- dedicated immutable rows; they never enter the generic media pipeline where
-- a browser might sniff or render them on the social application's origin.
CREATE TABLE webxdc_sessions (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL UNIQUE REFERENCES accounts (id) ON DELETE CASCADE,
    coordinator_uri text NOT NULL UNIQUE,
    creator_account_id bigint REFERENCES accounts (id) ON DELETE SET NULL,
    creator_uri text NOT NULL,
    name text NOT NULL,
    summary text NOT NULL DEFAULT '',
    bundle_id text NOT NULL,
    bundle_url text NOT NULL,
    bundle_name text NOT NULL,
    bundle_media_type text NOT NULL,
    digest_multibase text NOT NULL,
    bundle_bytes bytea NOT NULL,
    send_update_interval integer NOT NULL DEFAULT 10000,
    send_update_max_size integer NOT NULL DEFAULT 128000,
    membership_policy text NOT NULL DEFAULT 'open',
    last_serial bigint NOT NULL DEFAULT 0,
    published_at timestamptz NOT NULL DEFAULT now(),
    ended_at timestamptz,
    CHECK (bundle_media_type IN ('application/webxdc+zip', 'application/x-webxdc')),
    CHECK (send_update_interval >= 0),
    CHECK (send_update_max_size > 0),
    CHECK (membership_policy IN ('open', 'approval')),
    CHECK (last_serial >= 0 AND last_serial <= 9007199254740991)
);

-- Archive entries are expanded only after digest and ZIP validation.  Paths
-- are canonical, relative UTF-8 names; symlinks and duplicate names never
-- reach this table.
CREATE TABLE webxdc_files (
    session_id bigint NOT NULL REFERENCES webxdc_sessions (id) ON DELETE CASCADE,
    path text NOT NULL,
    media_type text NOT NULL,
    bytes bytea NOT NULL,
    PRIMARY KEY (session_id, path)
);

-- Membership is deliberately separate from generic follows: this collection
-- authorizes executable bundle access and durable fan-out, so a passive Group
-- follower must never enter it accidentally.
CREATE TABLE webxdc_memberships (
    session_id bigint NOT NULL REFERENCES webxdc_sessions (id) ON DELETE CASCADE,
    participant_account_id bigint NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    participant_uri text NOT NULL,
    follow_id text NOT NULL,
    accepted boolean NOT NULL DEFAULT false,
    replay_boundary bigint NOT NULL DEFAULT 0,
    self_addr text NOT NULL,
    joined_at timestamptz NOT NULL DEFAULT now(),
    accepted_at timestamptz,
    last_submitted_at timestamptz,
    PRIMARY KEY (session_id, participant_account_id),
    UNIQUE (session_id, participant_uri),
    UNIQUE (follow_id),
    CHECK (replay_boundary >= 0 AND replay_boundary <= 9007199254740991)
);

CREATE INDEX idx_webxdc_memberships_participant
    ON webxdc_memberships (participant_account_id, accepted);

-- The complete participant-authored Create is retained verbatim alongside the
-- JSON-literal update.  Both identifiers are idempotency keys and the serial
-- key enforces the coordinator's contiguous single order.
CREATE TABLE webxdc_updates (
    session_id bigint NOT NULL REFERENCES webxdc_sessions (id) ON DELETE CASCADE,
    serial bigint NOT NULL,
    create_id text NOT NULL,
    object_id text NOT NULL,
    actor_uri text NOT NULL,
    raw_create jsonb NOT NULL,
    webxdc_update jsonb NOT NULL,
    announce_id text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, serial),
    UNIQUE (create_id),
    UNIQUE (object_id),
    UNIQUE (announce_id),
    CHECK (serial > 0 AND serial <= 9007199254740991)
);

-- The normal status remains the backward-compatible invitation.  This sidecar
-- adds its audience and attached-link relation to every rebuilt Note.
CREATE TABLE webxdc_invitations (
    status_id bigint PRIMARY KEY REFERENCES statuses (id) ON DELETE CASCADE,
    session_id bigint NOT NULL REFERENCES webxdc_sessions (id) ON DELETE CASCADE
);
