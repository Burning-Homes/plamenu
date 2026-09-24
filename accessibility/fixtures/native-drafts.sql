-- Persisted-state UI fixture only, not upload/worker/federation evidence.
-- Run only in the disposable accessibility database, before starting its server.
\set ON_ERROR_STOP on
BEGIN;
INSERT INTO accounts (id, username, display_name, public_key, actor_type)
SELECT 8999, 'draft_fixture_films', 'Draft fixture films', public_key, 'Group'
FROM accounts WHERE username = 'developer' AND domain IS NULL;
INSERT INTO video_channels (account_id, owner_account_id)
SELECT 8999, id FROM accounts WHERE username = 'developer' AND domain IS NULL;
CREATE TEMP TABLE draft_fixture AS
SELECT 9000 + row_number() OVER (ORDER BY project, phase) AS id, project, phase,
    a.id AS owner
FROM (VALUES ('chromium-desktop'), ('chromium-mobile'), ('firefox-desktop'),
    ('firefox-mobile'), ('webkit-desktop'), ('webkit-mobile'), ('chromium-no-javascript')) projects(project)
CROSS JOIN (VALUES ('ready'), ('failed')) phases(phase)
CROSS JOIN accounts a WHERE a.username = 'developer' AND a.domain IS NULL;
INSERT INTO media_attachments (id, account_id, file_name, small_file_name, content_type,
    kind, processing, file_size, thumbnail_file_size, width, height, small_width, small_height,
    description, transcript, caption_vtt, duration)
SELECT id, owner, id || '.draft.mp4', id || '.draft.small.avif', 'video/mp4', 'video',
    CASE WHEN phase = 'ready' THEN 'complete' ELSE 'failed' END,
    :source_bytes, :poster_bytes, 320, 240, 320, 240,
    'A moving colour test pattern.', 'A steady test tone plays throughout.',
    E'WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n[Steady test tone]\n', 3
FROM draft_fixture;
INSERT INTO video_drafts (media_id, account_id, channel_id, title, description, language,
    visibility, source_file, source_cleanup_job_id, source_bytes, reserved_bytes, state, generation)
SELECT id, owner, 8999, 'Draft ' || project || ' ' || phase, 'Persisted-state browser fixture.',
    'en', 'unlisted', id || '.orig', 100000 + id * 10, :source_bytes, :source_bytes, phase, 1
FROM draft_fixture;
INSERT INTO video_draft_outputs (file_name, media_id, generation, cleanup_job_id)
SELECT id || suffix, id, 1, 100000 + id * 10 + offset_id
FROM draft_fixture CROSS JOIN (VALUES ('.draft.mp4', 1), ('.draft.small.avif', 2), ('.draft.poster.jpg', 3)) files(suffix, offset_id);
INSERT INTO video_storage_reservations (file_name, account_id, media_id, reserved_bytes)
SELECT id || suffix, owner, id, size
FROM draft_fixture CROSS JOIN (VALUES ('.orig', :source_bytes), ('.draft.mp4', :source_bytes),
    ('.draft.small.avif', :poster_bytes), ('.draft.poster.jpg', 1048576)) files(suffix, size);
COMMIT;
