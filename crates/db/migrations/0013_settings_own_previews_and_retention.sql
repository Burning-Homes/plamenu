-- The anonymous-access switches (timeline previews + public search) move from
-- plamenu.toml into the admin settings, keeping their private-by-default
-- posture. The O4 override columns whose config-file keys were removed at the
-- same time (emit_integrity_proofs, emit_rfc9421, ip_retention_days,
-- media_cache_retention_days) stop being nullable follow-the-file overrides
-- and become plain values with the defaults the file used to provide —
-- except media_cache_retention_days, whose default moves from 0 (keep
-- forever) to 14 days.
ALTER TABLE instance_settings
    ADD COLUMN timeline_preview_federated boolean NOT NULL DEFAULT false,
    ADD COLUMN timeline_preview_local boolean NOT NULL DEFAULT false,
    ADD COLUMN timeline_preview_tag boolean NOT NULL DEFAULT false,
    ADD COLUMN public_search boolean NOT NULL DEFAULT false;

UPDATE instance_settings SET
    emit_integrity_proofs = COALESCE(emit_integrity_proofs, true),
    emit_rfc9421 = COALESCE(emit_rfc9421, true),
    ip_retention_days = COALESCE(ip_retention_days, 365),
    media_cache_retention_days = COALESCE(media_cache_retention_days, 14);

ALTER TABLE instance_settings
    ALTER COLUMN emit_integrity_proofs SET DEFAULT true,
    ALTER COLUMN emit_integrity_proofs SET NOT NULL,
    ALTER COLUMN emit_rfc9421 SET DEFAULT true,
    ALTER COLUMN emit_rfc9421 SET NOT NULL,
    ALTER COLUMN ip_retention_days SET DEFAULT 365,
    ALTER COLUMN ip_retention_days SET NOT NULL,
    ALTER COLUMN media_cache_retention_days SET DEFAULT 14,
    ALTER COLUMN media_cache_retention_days SET NOT NULL;
