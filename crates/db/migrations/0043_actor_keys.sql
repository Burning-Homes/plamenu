-- Normalized ActivityPub verification/signing keys. Private material is
-- application ciphertext only; the encryption root never enters PostgreSQL.
CREATE TABLE actor_keys (
    id bigint PRIMARY KEY,
    owner_kind text NOT NULL CHECK (owner_kind IN ('account', 'instance')),
    account_id bigint REFERENCES accounts(id) ON DELETE CASCADE,
    key_uri text NOT NULL UNIQUE,
    controller_uri text NOT NULL,
    algorithm text NOT NULL CHECK (algorithm IN ('rsa', 'ed25519', 'ml-dsa-44')),
    public_key text NOT NULL,
    encrypted_private_key text,
    encryption_key_version integer,
    source text NOT NULL DEFAULT 'multikey' CHECK (source <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    activated_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz,
    revoked_at timestamptz,
    retired_at timestamptz,
    CONSTRAINT actor_keys_owner_shape CHECK (
        (owner_kind = 'account' AND account_id IS NOT NULL)
        OR (owner_kind = 'instance' AND account_id IS NULL)
    ),
    CONSTRAINT actor_keys_private_envelope CHECK (
        (encrypted_private_key IS NULL AND encryption_key_version IS NULL)
        OR (encrypted_private_key IS NOT NULL AND encryption_key_version IS NOT NULL)
    )
);

CREATE INDEX idx_actor_keys_account ON actor_keys (account_id, algorithm, id);
CREATE INDEX idx_actor_keys_usable_account ON actor_keys (account_id, algorithm, activated_at)
    WHERE revoked_at IS NULL AND retired_at IS NULL;
CREATE UNIQUE INDEX idx_actor_keys_instance_uri ON actor_keys (key_uri)
    WHERE owner_kind = 'instance';

-- A local actor may overlap old/new keys during rotation, but a single key URI
-- always identifies exactly one immutable public key. `key_uri`'s global
-- uniqueness prevents cross-controller substitution.
