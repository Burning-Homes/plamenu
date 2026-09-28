-- Numeric licence IDs do not identify a Creative Commons version. Only an
-- explicit canonical URL or a local author's versioned choice conveys it.
ALTER TABLE video_metadata ADD COLUMN licence_version_known boolean NOT NULL DEFAULT false;
