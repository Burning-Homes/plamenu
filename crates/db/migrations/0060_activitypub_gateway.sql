-- FEP-ae97 client-side signing gateway. Portable actors are represented by
-- hidden account rows so the existing delivery queue/key store can sign on
-- their behalf, while their client-signed documents remain authoritative and
-- are served without rewriting.
CREATE TABLE gateway_actors (
    account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    actor_uri text NOT NULL UNIQUE,
    inbox_uri text NOT NULL UNIQUE,
    outbox_uri text NOT NULL UNIQUE,
    username text NOT NULL,
    actor jsonb NOT NULL,
    client_rsa_key_id text NOT NULL,
    client_rsa_public_key text NOT NULL,
    gateway_rsa_public_multikey text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX gateway_actors_handle
    ON gateway_actors (lower(username));

-- Both inbox polling and outbox history use the same insertion-ordered store.
-- A globally unique object URI prevents a client from reusing an activity ID
-- in a different collection, as required by FEP-ae97.
CREATE TABLE gateway_collection_items (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES gateway_actors(account_id) ON DELETE CASCADE,
    collection text NOT NULL CHECK (collection IN ('inbox', 'outbox')),
    object_uri text NOT NULL UNIQUE,
    object jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX gateway_collection_items_poll
    ON gateway_collection_items (account_id, collection, id DESC);

-- Fetchable activities and embedded objects. The owning portable account is
-- kept beside every document so one DID cannot publish another DID's object.
CREATE TABLE gateway_objects (
    object_uri text PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES gateway_actors(account_id) ON DELETE CASCADE,
    object jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Gateway-side follower projection used only for followers-collection fanout.
-- The signed Follow/Accept activities remain the source of truth for clients.
CREATE TABLE gateway_followers (
    account_id bigint NOT NULL REFERENCES gateway_actors(account_id) ON DELETE CASCADE,
    actor_uri text NOT NULL,
    inbox_url text NOT NULL,
    accepted boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, actor_uri)
);

-- Multiple actors may upload identical content. The file is removed only when
-- the final owner row disappears.
CREATE TABLE gateway_media (
    account_id bigint NOT NULL REFERENCES gateway_actors(account_id) ON DELETE CASCADE,
    digest bytea NOT NULL,
    file_name text NOT NULL,
    content_type text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, digest)
);

CREATE INDEX gateway_media_digest ON gateway_media (digest);
