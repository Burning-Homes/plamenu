CREATE TABLE webxdc_settings (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    bundle_mb integer NOT NULL DEFAULT 256 CHECK (bundle_mb BETWEEN 1 AND 512),
    expanded_mb integer NOT NULL DEFAULT 512 CHECK (expanded_mb BETWEEN 1 AND 1024),
    file_mb integer NOT NULL DEFAULT 256 CHECK (file_mb BETWEEN 1 AND 512),
    session_mb integer NOT NULL DEFAULT 1024 CHECK (session_mb BETWEEN 1 AND 65536),
    account_mb integer NOT NULL DEFAULT 1024 CHECK (account_mb BETWEEN 1 AND 1048576),
    CHECK (file_mb <= expanded_mb),
    CHECK (session_mb >= bundle_mb + expanded_mb),
    CHECK (account_mb >= session_mb)
);
INSERT INTO webxdc_settings (singleton) VALUES (true);
