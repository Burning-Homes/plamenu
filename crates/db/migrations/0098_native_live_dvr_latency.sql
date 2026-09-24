-- Bounded live rewind and publisher-independent latency preference. Existing
-- broadcasts retain their old short live window; new ones choose explicitly.
ALTER TABLE native_live_broadcasts
    ADD COLUMN dvr_window_seconds INTEGER NOT NULL DEFAULT 0
        CHECK (dvr_window_seconds IN (0, 300, 600, 900, 1800)),
    ADD COLUMN latency_mode INTEGER NOT NULL DEFAULT 1
        CHECK (latency_mode IN (1, 2, 3));
