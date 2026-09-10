-- Remote-history images are metadata-only at ingest, but they use the ordinary
-- image proxy/cache lane when a viewer actually requests them.  Migration 0048
-- also set `download_on_demand`, whose established meaning is the separate
-- play-triggered A/V lane; that made the image-preview proxy look for a video
-- poster and strand images without one.
--
-- Keep `history_deferred` as the ingest-suppression/promotion marker and reserve
-- `download_on_demand` for audio/video.  This repairs rows created while 0048
-- was live; new writes enforce the distinction in application code.
UPDATE media_attachments
SET download_on_demand = false
WHERE history_deferred
  AND (kind = 'image' OR content_type LIKE 'image/%');
