-- Dedicated anonymous-access knobs for the discovery surfaces, which until
-- now all rode the public-timeline previews. Default posture: the local
-- timeline, trending, the local people directory and the local groups
-- directory are browsable signed out; anything federated (the federated
-- timeline, remote profiles in the directory) stays behind sign-in until the
-- operator opts in.
ALTER TABLE instance_settings
    ADD COLUMN anon_trends boolean NOT NULL DEFAULT true,
    ADD COLUMN anon_directory boolean NOT NULL DEFAULT true,
    ADD COLUMN anon_directory_federated boolean NOT NULL DEFAULT false,
    ADD COLUMN anon_groups boolean NOT NULL DEFAULT true,
    ALTER COLUMN timeline_preview_local SET DEFAULT true;
