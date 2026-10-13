-- Survives process death and folder/account erasure long enough to reconcile
-- the exact operator-owned inode. No cascade may discard filesystem intent.
CREATE TABLE music_local_mutations (
    id BIGINT PRIMARY KEY,
    folder_id BIGINT NOT NULL UNIQUE,
    file_id BIGINT NOT NULL,
    owner_id BIGINT NOT NULL,
    intent JSONB NOT NULL CHECK(jsonb_typeof(intent)='object'),
    applied BOOLEAN NOT NULL DEFAULT false,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX music_local_mutations_due ON music_local_mutations(next_attempt_at,id);
