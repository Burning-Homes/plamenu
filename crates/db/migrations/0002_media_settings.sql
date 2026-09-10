-- M39 media consolidation: operator knobs for media processing, animated
-- handling, encoder parameters, upload caps and per-class cache retention.
-- Defaults preserve pre-migration behavior except the two deliberate fixes:
--   * media_remote_gif_handling 'keep' — cached remote animated GIFs stay
--     GIFs (they used to be re-encoded to H.264 gifv, which destroyed
--     transparency and pixel-art quality; peers serve originals).
--   * gifv classification of local soundless videos now honours
--     media_gifv_max_seconds like the remote path always did.

ALTER TABLE instance_settings
    -- Animated media
    ADD COLUMN media_local_gif_handling text NOT NULL DEFAULT 'gifv',
    ADD COLUMN media_remote_gif_handling text NOT NULL DEFAULT 'keep',
    ADD COLUMN media_gifv_max_seconds integer NOT NULL DEFAULT 60,
    -- Still-image encoder parameters
    ADD COLUMN media_avif_quality integer NOT NULL DEFAULT 60,
    ADD COLUMN media_avif_speed_full integer NOT NULL DEFAULT 10,
    ADD COLUMN media_avif_speed_preview integer NOT NULL DEFAULT 6,
    ADD COLUMN media_jpeg_quality integer NOT NULL DEFAULT 85,
    ADD COLUMN media_max_edge integer NOT NULL DEFAULT 1920,
    -- JPEG XL (opt-in value of the full-processing modes; needs ffmpeg
    -- built with libjxl — the official image has it)
    ADD COLUMN media_jxl_distance real NOT NULL DEFAULT 1.0,
    ADD COLUMN media_jxl_effort integer NOT NULL DEFAULT 4,
    -- Video/audio transcode parameters (local uploads; remote video is
    -- remux-only by design)
    ADD COLUMN media_video_preset text NOT NULL DEFAULT 'veryfast',
    ADD COLUMN media_video_rate_mode text NOT NULL DEFAULT 'abr',
    ADD COLUMN media_video_crf integer NOT NULL DEFAULT 23,
    ADD COLUMN media_audio_bitrate_kbps integer NOT NULL DEFAULT 192,
    -- Upload caps (bounded above by the compiled-in router body limits)
    ADD COLUMN media_max_image_mb integer NOT NULL DEFAULT 16,
    ADD COLUMN media_max_av_mb integer NOT NULL DEFAULT 99,
    -- Heavy-work concurrency (0 = auto: half the cores; applied at boot)
    ADD COLUMN media_processing_concurrency integer NOT NULL DEFAULT 0,
    -- Cache retention grades. NULL media_video_retention_days follows
    -- media_cache_retention_days; 0 = keep forever. Size caps in GiB,
    -- 0 = unlimited, evicting oldest first (HLS: least recently used).
    ADD COLUMN media_video_retention_days integer,
    ADD COLUMN media_profile_retention_days integer NOT NULL DEFAULT 0,
    ADD COLUMN media_card_retention_days integer NOT NULL DEFAULT 0,
    ADD COLUMN media_emoji_retention_days integer NOT NULL DEFAULT 0,
    ADD COLUMN media_cache_max_gb integer NOT NULL DEFAULT 0,
    ADD COLUMN media_video_cache_max_gb integer NOT NULL DEFAULT 0;
