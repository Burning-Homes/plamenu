-- O4: operator-editable overrides for federation posture knobs that were
-- config-file-only. NULL = follow the plamenu.toml bootstrap default (the
-- media_cache_retention_days precedent); a non-NULL value wins and applies
-- live (settings-cache TTL) without a restart.
ALTER TABLE instance_settings
    ADD COLUMN authorized_fetch boolean,
    ADD COLUMN authorized_fetch_unsigned_profile boolean,
    ADD COLUMN emit_integrity_proofs boolean,
    ADD COLUMN emit_rfc9421 boolean,
    ADD COLUMN conversation_containers boolean,
    ADD COLUMN ip_retention_days integer;
