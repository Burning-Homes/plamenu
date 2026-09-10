-- Bind an OAuth authorization request to the second-factor challenge it
-- spawned (QC audit #34). The password leg stores the reviewed client /
-- callback / scopes / state / PKCE values here, and the TOTP and WebAuthn legs
-- mint the grant from THESE server-held values — never from the resubmitted
-- form — so a captured challenge token cannot finish a different authorization
-- request than the one the user proved their password for. Typed columns, not
-- JSON: this is fixed-shape application data (see the schema contract test on
-- reviewed JSON columns). All five are NULL outside the OAuth context;
-- `oauth_client_id` is the presence marker.
ALTER TABLE two_factor_challenges
    ADD COLUMN oauth_client_id text,
    ADD COLUMN oauth_redirect_uri text,
    ADD COLUMN oauth_scope text,
    ADD COLUMN oauth_state text,
    ADD COLUMN oauth_code_challenge text;
