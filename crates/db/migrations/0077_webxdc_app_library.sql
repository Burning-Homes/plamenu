-- Reusable Webxdc apps are mutable library identities whose versions point at
-- the same immutable, digest-addressed packages used by sessions.  A personal
-- app is identified by owner_account_id; an ownerless app belongs to the
-- instance library and has an explicit moderation/publication state.
ALTER TABLE webxdc_settings ADD COLUMN personal_apps integer NOT NULL DEFAULT 50
    CHECK (personal_apps BETWEEN 0 AND 10000);

CREATE TABLE webxdc_catalog_sources (
    id bigint PRIMARY KEY,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    feed_url text NOT NULL UNIQUE,
    adapter text NOT NULL DEFAULT 'xdcget-v1' CHECK (adapter IN ('xdcget-v1')),
    enabled boolean NOT NULL DEFAULT true,
    last_fetched_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE webxdc_apps (
    id bigint PRIMARY KEY,
    owner_account_id bigint REFERENCES accounts(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    summary text NOT NULL DEFAULT '' CHECK (char_length(summary) <= 2000),
    category text CHECK (char_length(category) BETWEEN 1 AND 80),
    visibility text NOT NULL DEFAULT 'private'
        CHECK (visibility IN ('private', 'hidden', 'instance', 'public')),
    source_kind text NOT NULL DEFAULT 'upload'
        CHECK (source_kind IN ('upload', 'promotion', 'external', 'federated')),
    source_url text,
    catalog_source_id bigint REFERENCES webxdc_catalog_sources(id) ON DELETE SET NULL,
    external_app_id text,
    promoted_from_app_id bigint REFERENCES webxdc_apps(id) ON DELETE SET NULL,
    created_by_account_id bigint REFERENCES accounts(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((owner_account_id IS NOT NULL AND visibility = 'private')
        OR (owner_account_id IS NULL AND visibility <> 'private')),
    CHECK ((source_kind = 'external' AND external_app_id IS NOT NULL)
        OR source_kind <> 'external')
);
CREATE UNIQUE INDEX webxdc_apps_external_identity
    ON webxdc_apps(catalog_source_id, external_app_id)
    WHERE catalog_source_id IS NOT NULL AND external_app_id IS NOT NULL
      AND owner_account_id IS NULL;
CREATE UNIQUE INDEX webxdc_apps_personal_external_identity
    ON webxdc_apps(owner_account_id, catalog_source_id, external_app_id)
    WHERE owner_account_id IS NOT NULL AND catalog_source_id IS NOT NULL
      AND external_app_id IS NOT NULL;
CREATE UNIQUE INDEX webxdc_apps_one_promotion
    ON webxdc_apps(promoted_from_app_id)
    WHERE promoted_from_app_id IS NOT NULL AND owner_account_id IS NULL;
CREATE INDEX webxdc_apps_personal_list
    ON webxdc_apps(owner_account_id, lower(name), id)
    WHERE owner_account_id IS NOT NULL;
CREATE INDEX webxdc_apps_instance_list
    ON webxdc_apps(lower(name), id)
    WHERE owner_account_id IS NULL;
CREATE INDEX webxdc_apps_public_catalog
    ON webxdc_apps(lower(name), id)
    WHERE owner_account_id IS NULL AND visibility = 'public';

CREATE TABLE webxdc_app_versions (
    id bigint PRIMARY KEY,
    app_id bigint NOT NULL REFERENCES webxdc_apps(id) ON DELETE CASCADE,
    digest_multibase text NOT NULL REFERENCES webxdc_packages(digest_multibase),
    version text NOT NULL DEFAULT '' CHECK (char_length(version) <= 120),
    filename text NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255),
    manifest_name text NOT NULL CHECK (char_length(manifest_name) BETWEEN 1 AND 120),
    source_code_url text,
    icon_path text CHECK (icon_path IN ('icon.png', 'icon.jpg')),
    source_url text,
    created_by_account_id bigint REFERENCES accounts(id) ON DELETE SET NULL,
    current boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (app_id, digest_multibase)
);
CREATE UNIQUE INDEX webxdc_app_versions_one_current
    ON webxdc_app_versions(app_id) WHERE current;
CREATE INDEX webxdc_app_versions_package
    ON webxdc_app_versions(digest_multibase);

-- The xdcget adapter caches advisory discovery metadata.  Import always
-- downloads and revalidates the package; these rows never authorize execution.
CREATE TABLE webxdc_catalog_candidates (
    source_id bigint NOT NULL REFERENCES webxdc_catalog_sources(id) ON DELETE CASCADE,
    external_app_id text NOT NULL CHECK (char_length(external_app_id) BETWEEN 1 AND 240),
    version text NOT NULL DEFAULT '' CHECK (char_length(version) <= 120),
    bundle_url text NOT NULL,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    summary text NOT NULL DEFAULT '' CHECK (char_length(summary) <= 2000),
    category text CHECK (char_length(category) BETWEEN 1 AND 80),
    source_code_url text,
    advertised_size bigint CHECK (advertised_size IS NULL OR advertised_size >= 0),
    published_at timestamptz,
    seen_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (source_id, external_app_id)
);
CREATE INDEX webxdc_catalog_candidates_list
    ON webxdc_catalog_candidates(source_id, lower(name), external_app_id);

-- Package lifetime now follows both sessions and reusable versions.  The
-- advisory lock is shared with package reservation, so a concurrent session or
-- library insertion cannot lose the package between its check and reference.
CREATE OR REPLACE FUNCTION webxdc_release_package() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(752, 1);
    DELETE FROM webxdc_packages p WHERE p.digest_multibase=OLD.digest_multibase
      AND NOT EXISTS (SELECT 1 FROM webxdc_sessions s
                      WHERE s.digest_multibase=p.digest_multibase)
      AND NOT EXISTS (SELECT 1 FROM webxdc_app_versions v
                      WHERE v.digest_multibase=p.digest_multibase);
    RETURN NULL;
END;
$$;
CREATE TRIGGER webxdc_release_library_package AFTER DELETE ON webxdc_app_versions
FOR EACH ROW EXECUTE FUNCTION webxdc_release_package();
