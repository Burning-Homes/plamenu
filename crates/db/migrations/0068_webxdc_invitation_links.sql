-- Preserve remote invitation display metadata without resolving/downloading apps.
CREATE TABLE webxdc_invitation_links (
    status_id bigint PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    session_uri text NOT NULL,
    session_name text NOT NULL
);
