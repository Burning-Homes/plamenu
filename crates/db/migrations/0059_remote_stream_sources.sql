-- Provider-specific discovery data for federated live-stream actors.
--
-- Owncast deliberately federates the go-live announcement as an ordinary
-- Note. Its playable HLS endpoint is instead advertised in WebFinger and its
-- current online state lives at the public `/api/status` endpoint. Keeping
-- that positively-detected capability beside the account lets the normal
-- Follow/Note flow stay untouched while the media layer can render it as the
-- live video it represents.
CREATE TABLE remote_stream_sources (
    account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    media_id bigint NOT NULL UNIQUE REFERENCES media_attachments(id) ON DELETE CASCADE,
    provider text NOT NULL CHECK (provider IN ('owncast')),
    homepage_url text NOT NULL UNIQUE,
    status_url text NOT NULL,
    hls_master_url text NOT NULL,
    created_at timestamp with time zone NOT NULL DEFAULT now(),
    updated_at timestamp with time zone NOT NULL DEFAULT now()
);
