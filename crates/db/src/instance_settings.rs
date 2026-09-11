//! Operator-editable instance settings — Mastodon's `Setting` key/value rows
//! (`site_title`, `site_short_description`, …), stored as one typed row per
//! project convention. Seeded by the migration, so [`get`] always finds it.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// Mastodon's `Setting.registrations_mode`: who may self-register.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RegistrationsMode {
    Open,
    Approved,
    /// Nobody (invites still work) — the default, matching Mastodon's new
    /// installs and the pre-M21 CLI-only state.
    #[default]
    None,
}

impl RegistrationsMode {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "open" => Self::Open,
            "approved" => Self::Approved,
            _ => Self::None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Approved => "approved",
            Self::None => "none",
        }
    }
}

/// Mastodon's `show_domain_blocks` / `show_domain_blocks_rationale`
/// audience ladder: nobody, signed-in local users, or everyone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DomainBlocksDisclosure {
    #[default]
    Disabled,
    Users,
    All,
}

impl DomainBlocksDisclosure {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "users" => Self::Users,
            "all" => Self::All,
            _ => Self::Disabled,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Users => "users",
            Self::All => "all",
        }
    }
}

/// Who may create local groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GroupCreationPolicy {
    /// Staff only.
    Admins,
    /// Staff plus accounts an admin has approved. Until the per-account
    /// grant queue ships (admin console), enforcement equals `Admins`.
    Approved,
    #[default]
    Everyone,
}

impl GroupCreationPolicy {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "admins" => Self::Admins,
            "approved" => Self::Approved,
            _ => Self::Everyone,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admins => "admins",
            Self::Approved => "approved",
            Self::Everyone => "everyone",
        }
    }
}

// Independent operator toggles, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceSettings {
    pub site_title: String,
    pub site_short_description: String,
    pub site_extended_description: String,
    pub site_contact_username: String,
    pub site_contact_email: String,
    pub custom_css: String,
    pub registrations_mode: String,
    pub rate_limiting_enabled: bool,
    pub rate_limit_authenticated_api: i32,
    pub rate_limit_per_token_api: i32,
    pub rate_limit_unauthenticated_api: i32,
    pub rate_limit_api_media: i32,
    pub rate_limit_api_delete: i32,
    pub rate_limit_api_sign_up: i32,
    pub rate_limit_app_registrations: i32,
    pub rate_limit_paging: i32,
    pub rate_limit_login_attempts: i32,
    pub rate_limit_password_resets: i32,
    pub rate_limit_sign_up_web: i32,
    pub max_characters: i32,
    /// The cap a long-form (`Article`) post is measured against instead of
    /// [`Self::max_characters`]: ordinary posts default to 5,000 characters;
    /// long-form posts have their own larger cap.
    pub max_characters_long_form: i32,
    pub max_media_attachments: i32,
    pub poll_max_options: i32,
    pub activity_api_enabled: bool,
    pub peers_api_enabled: bool,
    pub profile_directory: bool,
    pub trends_enabled: bool,
    pub trendable_by_default: bool,
    /// Serve the public welcome page at `/` to signed-out visitors; off
    /// restores the redirect to `/login`.
    pub landing_page: bool,
    /// The landing page's stats block, switchable on its own.
    pub landing_show_stats: bool,
    pub show_domain_blocks: String,
    pub show_domain_blocks_rationale: String,
    /// Minimum sign-up age, Mastodon's `Setting.min_age`. `0` (default)
    /// leaves the age gate off; a positive value requires a date of birth at
    /// least that old to register.
    pub min_age: i32,
    /// Days a cached copy of remote media is kept before eviction
    /// (Mastodon's `media_cache_retention_period`); `0` = eviction off.
    pub media_cache_retention_days: i32,
    /// How the full/original rendition of a **local** status image upload is
    /// stored: `passthrough` (default — as received, metadata stripped),
    /// `avif`, or `jpeg`. Parsed into `media_processing::FullMedia` in the
    /// server crate.
    pub media_full_processing: String,
    /// How the `small`/preview rendition is encoded: `avif` (default) or
    /// `jpeg`. Parsed into `media_processing::PreviewMedia`. Applies to local
    /// uploads and cached remote attachments alike.
    pub media_preview_processing: String,
    /// How the full rendition of a **cached remote status attachment** is
    /// stored — defaults to `avif`, independently of the local-upload setting
    /// `media_full_processing`.
    pub media_remote_full_processing: String,
    /// How cached copies of incoming emoji, avatar/header and preview-card
    /// images are stored: `avif` (default — they are served to local clients
    /// only, never re-federated, so AVIF is safe here) or `passthrough`
    /// (as arrived). Animated images are always kept as arrived. Parsed into
    /// `media_processing::CachedImage`.
    pub media_cached_image_processing: String,
    /// Largest remote video (all streams together) worth caching, in MiB —
    /// the budget for on-demand long-form downloads (`PeerTube`). `0` turns
    /// remote-video caching off (playback then relies on the viewer's
    /// direct-remote preference).
    pub remote_video_max_mb: i32,
    /// Tallest rendition to pick when a remote video offers several
    /// (`PeerTube`'s `url` tree carries one file Link per resolution).
    pub remote_video_max_height: i32,
    /// Local animated-GIF uploads: `gifv` (default — soundless H.264 mp4 like
    /// Mastodon, except GIFs with transparency stay GIFs, which H.264 cannot
    /// carry) or `keep` (store the GIF as uploaded, like Pleroma/Misskey).
    pub media_local_gif_handling: String,
    /// Cached remote animated GIFs: `keep` (the origin's bytes;
    /// re-encoding cost peers' pixel art its quality and transparency),
    /// `gifv` (H.264 mp4 for opaque GIFs, keep transparent ones), or `webp`
    /// (default: animated WebP — alpha-safe and smaller, needs ffmpeg `libwebp_anim`).
    pub media_remote_gif_handling: String,
    /// Longest soundless video still classified `gifv` (auto-playing,
    /// looping), in seconds; applies to local uploads and remote caching.
    pub media_gifv_max_seconds: i32,
    /// AVIF encoder quality (1-100) for every AVIF rendition.
    pub media_avif_quality: i32,
    /// AVIF speed (1-10, higher = faster/larger) for full renditions. This is
    /// mapped onto libaom's `cpu-used` range and is fast by default because
    /// proxy cache misses encode inline.
    pub media_avif_speed_full: i32,
    /// AVIF/libaom speed for preview/`small` renditions (denser, off the hot
    /// path).
    pub media_avif_speed_preview: i32,
    /// JPEG encoder quality (1-100) for photo renditions.
    pub media_jpeg_quality: i32,
    /// Longest edge a re-encoded full rendition is downscaled to.
    pub media_max_edge: i32,
    /// JPEG XL Butteraugli distance (0.0 = mathematically lossless,
    /// 1.0 ≈ visually lossless) when a full-processing mode is `jxl`.
    pub media_jxl_distance: f32,
    /// JPEG XL encoder effort (1-9, higher = denser/slower).
    pub media_jxl_effort: i32,
    /// x264 preset for local video re-encodes.
    pub media_video_preset: String,
    /// Video rate control: `abr` (Mastodon's bits-per-pixel budget, default)
    /// or `crf` (constant quality, better bits-per-quality, unbounded size).
    pub media_video_rate_mode: String,
    /// x264 CRF (0-51) when `media_video_rate_mode` is `crf`.
    pub media_video_crf: i32,
    /// AAC bitrate (kbps) for transcoded video/audio tracks.
    pub media_audio_bitrate_kbps: i32,
    /// Largest accepted image upload, MiB (≤ the compiled router limit).
    pub media_max_image_mb: i32,
    /// Largest accepted video/audio upload, MiB (≤ the compiled router
    /// limit); also the size budget the ABR bitrate heuristic fits into.
    pub media_max_av_mb: i32,
    /// Concurrent heavy media jobs (ffmpeg re-encodes + full-size image
    /// encodes); `0` = auto (half the cores). Applied at boot.
    pub media_processing_concurrency: i32,
    /// Days cached remote *videos* (on-demand mp4 + HLS segments) are kept;
    /// `None` follows `media_cache_retention_days`, `0` keeps forever.
    pub media_video_retention_days: Option<i32>,
    /// Days cached remote avatars/headers are kept (refetched on demand);
    /// `0` keeps forever.
    pub media_profile_retention_days: i32,
    /// Days cached preview-card images are kept; `0` keeps forever.
    pub media_card_retention_days: i32,
    /// Days cached remote custom-emoji images are kept; `0` keeps forever.
    pub media_emoji_retention_days: i32,
    /// Total size cap (GiB) for cached remote attachments, oldest evicted
    /// first; `0` = unlimited.
    pub media_cache_max_gb: i32,
    /// Total size cap (GiB) for cached remote video (on-demand mp4 + HLS
    /// segments), least-recently-watched evicted first; `0` = unlimited.
    pub media_video_cache_max_gb: i32,
    /// Who may create local groups: `admins` | `approved` | `everyone`.
    pub group_creation_policy: String,
    /// Live override for `authorized_fetch` (secure mode — require HTTP
    /// signatures on `ActivityPub` GETs). `None` follows the config file,
    /// like the other remaining overrides (O4).
    pub authorized_fetch: Option<bool>,
    /// Live override for the unsigned-profile carve-out under secure mode
    /// (serve full actor documents to unsigned fetchers; Lemmy interop).
    pub authorized_fetch_unsigned_profile: Option<bool>,
    /// FEP-8b32 Ed25519 integrity proofs on deliveries.
    pub emit_integrity_proofs: bool,
    /// RFC 9421 double-knock delivery signatures.
    pub emit_rfc9421: bool,
    /// Live override for FEP-171b conversation-container ownership.
    pub conversation_containers: Option<bool>,
    /// How many days sign-in logs and stored IPs are kept.
    pub ip_retention_days: i32,
    /// Anonymous access to the federated timeline (API and web).
    pub timeline_preview_federated: bool,
    /// Anonymous access to the local timeline (API and web).
    pub timeline_preview_local: bool,
    /// Anonymous access to hashtag timelines (API and web).
    pub timeline_preview_tag: bool,
    /// Anonymous access to search (API and web).
    pub public_search: bool,
    /// Anonymous access to the Trending pages (web only; the trends API is
    /// public whenever trends are enabled).
    pub anon_trends: bool,
    /// Anonymous access to the People directory page (web only).
    pub anon_directory: bool,
    /// Whether anonymous visitors may widen the People directory to remote
    /// profiles; off, they only see this server's.
    pub anon_directory_federated: bool,
    /// Anonymous access to the Groups directory page (web only).
    pub anon_groups: bool,
    /// Whether the public and local timelines carry replies. Off (the default,
    /// and Mastodon's) they show originals and self-threads only; on restores
    /// the pre-0031 firehose. Hashtag, home and list timelines are unaffected.
    pub public_timeline_replies: bool,
    /// Whether the built-in web client merges repeated boosts of one post into
    /// a single card. Presentation only — the API always serves every
    /// boost row.
    pub boost_collapse: bool,
    /// How many posts above the rendered page the collapse looks back, so a
    /// boost of something already shown on an earlier page is suppressed
    /// instead of repeating. `0` (the default) collapses within the fetched
    /// page only and costs no extra query.
    pub boost_collapse_lookback: i32,
    /// Days an unused cached status translation is kept; `0` keeps forever.
    pub translation_cache_retention_days: i32,
    /// Hard row cap for the translation cache (LRU-evicted); `0` = uncapped.
    pub translation_cache_max_rows: i32,
    /// Concurrent requests allowed toward the translation backend — match a
    /// self-hosted backend's parallel slots; commercial APIs tolerate more.
    pub translation_backend_concurrency: i32,
    /// Per-account translations per hour that may *reach the backend*
    /// (cache hits are free and unmetered); `0` disables the limit.
    pub translation_user_rate_limit_per_hour: i32,
    /// Re-translate cached rows whose provider differs from the configured
    /// backend; off serves them with their stored attribution.
    pub translation_refresh_on_provider_change: bool,
    /// When the operator initiated server self-destruct (Mastodon's
    /// `tootctl self-destruct`); `None` in normal operation. One-way: set by
    /// [`begin_self_destruct`], never part of [`SettingsUpdate`].
    pub self_destruct_initiated_at: Option<OffsetDateTime>,
    /// When the maintenance worker last auto-reverted open registration to
    /// approval mode (no moderator active for a week) — the admin console
    /// explains the flip while this is set. Set by
    /// [`mark_registrations_auto_closed`]; cleared by every [`save`] (an
    /// operator touching the settings form has seen the notice), never part
    /// of [`SettingsUpdate`].
    pub registrations_auto_closed_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

impl InstanceSettings {
    #[must_use]
    pub fn registrations_mode(&self) -> RegistrationsMode {
        RegistrationsMode::parse(&self.registrations_mode)
    }

    #[must_use]
    pub fn show_domain_blocks(&self) -> DomainBlocksDisclosure {
        DomainBlocksDisclosure::parse(&self.show_domain_blocks)
    }

    #[must_use]
    pub fn show_domain_blocks_rationale(&self) -> DomainBlocksDisclosure {
        DomainBlocksDisclosure::parse(&self.show_domain_blocks_rationale)
    }

    #[must_use]
    pub fn group_creation_policy(&self) -> GroupCreationPolicy {
        GroupCreationPolicy::parse(&self.group_creation_policy)
    }

    /// Whether the server is winding down (self-destruct mode).
    #[must_use]
    pub fn is_self_destructing(&self) -> bool {
        self.self_destruct_initiated_at.is_some()
    }

    /// The row as a full-set update, for callers changing only some fields
    /// (`SettingsUpdate { registrations_mode: …, ..current.as_update() }`).
    #[must_use]
    pub fn as_update(&self) -> SettingsUpdate<'_> {
        SettingsUpdate {
            site_title: &self.site_title,
            site_short_description: &self.site_short_description,
            site_extended_description: &self.site_extended_description,
            site_contact_username: &self.site_contact_username,
            site_contact_email: &self.site_contact_email,
            custom_css: &self.custom_css,
            registrations_mode: self.registrations_mode(),
            rate_limiting_enabled: self.rate_limiting_enabled,
            rate_limit_authenticated_api: self.rate_limit_authenticated_api,
            rate_limit_per_token_api: self.rate_limit_per_token_api,
            rate_limit_unauthenticated_api: self.rate_limit_unauthenticated_api,
            rate_limit_api_media: self.rate_limit_api_media,
            rate_limit_api_delete: self.rate_limit_api_delete,
            rate_limit_api_sign_up: self.rate_limit_api_sign_up,
            rate_limit_app_registrations: self.rate_limit_app_registrations,
            rate_limit_paging: self.rate_limit_paging,
            rate_limit_login_attempts: self.rate_limit_login_attempts,
            rate_limit_password_resets: self.rate_limit_password_resets,
            rate_limit_sign_up_web: self.rate_limit_sign_up_web,
            max_characters: self.max_characters,
            max_characters_long_form: self.max_characters_long_form,
            max_media_attachments: self.max_media_attachments,
            poll_max_options: self.poll_max_options,
            activity_api_enabled: self.activity_api_enabled,
            peers_api_enabled: self.peers_api_enabled,
            profile_directory: self.profile_directory,
            trends_enabled: self.trends_enabled,
            trendable_by_default: self.trendable_by_default,
            landing_page: self.landing_page,
            landing_show_stats: self.landing_show_stats,
            show_domain_blocks: self.show_domain_blocks(),
            show_domain_blocks_rationale: self.show_domain_blocks_rationale(),
            min_age: self.min_age,
            media_cache_retention_days: self.media_cache_retention_days,
            media_full_processing: &self.media_full_processing,
            media_preview_processing: &self.media_preview_processing,
            media_remote_full_processing: &self.media_remote_full_processing,
            media_cached_image_processing: &self.media_cached_image_processing,
            remote_video_max_mb: self.remote_video_max_mb,
            remote_video_max_height: self.remote_video_max_height,
            media_local_gif_handling: &self.media_local_gif_handling,
            media_remote_gif_handling: &self.media_remote_gif_handling,
            media_gifv_max_seconds: self.media_gifv_max_seconds,
            media_avif_quality: self.media_avif_quality,
            media_avif_speed_full: self.media_avif_speed_full,
            media_avif_speed_preview: self.media_avif_speed_preview,
            media_jpeg_quality: self.media_jpeg_quality,
            media_max_edge: self.media_max_edge,
            media_jxl_distance: self.media_jxl_distance,
            media_jxl_effort: self.media_jxl_effort,
            media_video_preset: &self.media_video_preset,
            media_video_rate_mode: &self.media_video_rate_mode,
            media_video_crf: self.media_video_crf,
            media_audio_bitrate_kbps: self.media_audio_bitrate_kbps,
            media_max_image_mb: self.media_max_image_mb,
            media_max_av_mb: self.media_max_av_mb,
            media_processing_concurrency: self.media_processing_concurrency,
            media_video_retention_days: self.media_video_retention_days,
            media_profile_retention_days: self.media_profile_retention_days,
            media_card_retention_days: self.media_card_retention_days,
            media_emoji_retention_days: self.media_emoji_retention_days,
            media_cache_max_gb: self.media_cache_max_gb,
            media_video_cache_max_gb: self.media_video_cache_max_gb,
            group_creation_policy: self.group_creation_policy(),
            authorized_fetch: self.authorized_fetch,
            authorized_fetch_unsigned_profile: self.authorized_fetch_unsigned_profile,
            emit_integrity_proofs: self.emit_integrity_proofs,
            emit_rfc9421: self.emit_rfc9421,
            conversation_containers: self.conversation_containers,
            ip_retention_days: self.ip_retention_days,
            timeline_preview_federated: self.timeline_preview_federated,
            timeline_preview_local: self.timeline_preview_local,
            timeline_preview_tag: self.timeline_preview_tag,
            public_search: self.public_search,
            anon_trends: self.anon_trends,
            anon_directory: self.anon_directory,
            anon_directory_federated: self.anon_directory_federated,
            anon_groups: self.anon_groups,
            public_timeline_replies: self.public_timeline_replies,
            boost_collapse: self.boost_collapse,
            boost_collapse_lookback: self.boost_collapse_lookback,
            translation_cache_retention_days: self.translation_cache_retention_days,
            translation_cache_max_rows: self.translation_cache_max_rows,
            translation_backend_concurrency: self.translation_backend_concurrency,
            translation_user_rate_limit_per_hour: self.translation_user_rate_limit_per_hour,
            translation_refresh_on_provider_change: self.translation_refresh_on_provider_change,
        }
    }
}

/// The instance settings row (single, seeded by migration `0066`).
pub async fn get<'e, E: sqlx::PgExecutor<'e>>(pool: E) -> Result<InstanceSettings, DbError> {
    let settings = sqlx::query_as!(
        InstanceSettings,
        r#"
        SELECT site_title, site_short_description, site_extended_description,
               site_contact_username, site_contact_email, custom_css,
               registrations_mode, rate_limiting_enabled,
               rate_limit_authenticated_api, rate_limit_per_token_api,
               rate_limit_unauthenticated_api, rate_limit_api_media,
               rate_limit_api_delete, rate_limit_api_sign_up,
               rate_limit_app_registrations, rate_limit_paging,
               rate_limit_login_attempts, rate_limit_password_resets,
               rate_limit_sign_up_web, max_characters, max_characters_long_form,
               max_media_attachments,
               poll_max_options, activity_api_enabled, peers_api_enabled,
               profile_directory, trends_enabled, trendable_by_default,
               landing_page, landing_show_stats,
               show_domain_blocks, show_domain_blocks_rationale, min_age,
               media_cache_retention_days,
               media_full_processing, media_preview_processing,
               media_remote_full_processing, media_cached_image_processing,
               remote_video_max_mb, remote_video_max_height,
               media_local_gif_handling, media_remote_gif_handling,
               media_gifv_max_seconds, media_avif_quality,
               media_avif_speed_full, media_avif_speed_preview,
               media_jpeg_quality, media_max_edge,
               media_jxl_distance, media_jxl_effort,
               media_video_preset, media_video_rate_mode, media_video_crf,
               media_audio_bitrate_kbps, media_max_image_mb, media_max_av_mb,
               media_processing_concurrency, media_video_retention_days,
               media_profile_retention_days, media_card_retention_days,
               media_emoji_retention_days, media_cache_max_gb,
               media_video_cache_max_gb,
               group_creation_policy,
               authorized_fetch, authorized_fetch_unsigned_profile,
               emit_integrity_proofs, emit_rfc9421, conversation_containers,
               ip_retention_days,
               timeline_preview_federated, timeline_preview_local,
               timeline_preview_tag, public_search,
               anon_trends, anon_directory, anon_directory_federated,
               anon_groups, public_timeline_replies,
               boost_collapse, boost_collapse_lookback,
               translation_cache_retention_days, translation_cache_max_rows,
               translation_backend_concurrency,
               translation_user_rate_limit_per_hour,
               translation_refresh_on_provider_change,
               self_destruct_initiated_at,
               registrations_auto_closed_at, updated_at
        FROM instance_settings
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(settings)
}

/// The full operator-editable set, as submitted by the admin settings form.
#[allow(clippy::struct_excessive_bools)]
pub struct SettingsUpdate<'a> {
    pub site_title: &'a str,
    pub site_short_description: &'a str,
    pub site_extended_description: &'a str,
    pub site_contact_username: &'a str,
    pub site_contact_email: &'a str,
    pub custom_css: &'a str,
    pub registrations_mode: RegistrationsMode,
    pub rate_limiting_enabled: bool,
    pub rate_limit_authenticated_api: i32,
    pub rate_limit_per_token_api: i32,
    pub rate_limit_unauthenticated_api: i32,
    pub rate_limit_api_media: i32,
    pub rate_limit_api_delete: i32,
    pub rate_limit_api_sign_up: i32,
    pub rate_limit_app_registrations: i32,
    pub rate_limit_paging: i32,
    pub rate_limit_login_attempts: i32,
    pub rate_limit_password_resets: i32,
    pub rate_limit_sign_up_web: i32,
    pub max_characters: i32,
    pub max_characters_long_form: i32,
    pub max_media_attachments: i32,
    pub poll_max_options: i32,
    pub activity_api_enabled: bool,
    pub peers_api_enabled: bool,
    pub profile_directory: bool,
    pub trends_enabled: bool,
    pub trendable_by_default: bool,
    pub landing_page: bool,
    pub landing_show_stats: bool,
    pub show_domain_blocks: DomainBlocksDisclosure,
    pub show_domain_blocks_rationale: DomainBlocksDisclosure,
    pub min_age: i32,
    pub media_cache_retention_days: i32,
    pub media_full_processing: &'a str,
    pub media_preview_processing: &'a str,
    pub media_remote_full_processing: &'a str,
    pub media_cached_image_processing: &'a str,
    pub remote_video_max_mb: i32,
    pub remote_video_max_height: i32,
    pub media_local_gif_handling: &'a str,
    pub media_remote_gif_handling: &'a str,
    pub media_gifv_max_seconds: i32,
    pub media_avif_quality: i32,
    pub media_avif_speed_full: i32,
    pub media_avif_speed_preview: i32,
    pub media_jpeg_quality: i32,
    pub media_max_edge: i32,
    pub media_jxl_distance: f32,
    pub media_jxl_effort: i32,
    pub media_video_preset: &'a str,
    pub media_video_rate_mode: &'a str,
    pub media_video_crf: i32,
    pub media_audio_bitrate_kbps: i32,
    pub media_max_image_mb: i32,
    pub media_max_av_mb: i32,
    pub media_processing_concurrency: i32,
    pub media_video_retention_days: Option<i32>,
    pub media_profile_retention_days: i32,
    pub media_card_retention_days: i32,
    pub media_emoji_retention_days: i32,
    pub media_cache_max_gb: i32,
    pub media_video_cache_max_gb: i32,
    pub group_creation_policy: GroupCreationPolicy,
    pub authorized_fetch: Option<bool>,
    pub authorized_fetch_unsigned_profile: Option<bool>,
    pub emit_integrity_proofs: bool,
    pub emit_rfc9421: bool,
    pub conversation_containers: Option<bool>,
    pub ip_retention_days: i32,
    pub timeline_preview_federated: bool,
    pub timeline_preview_local: bool,
    pub timeline_preview_tag: bool,
    pub public_search: bool,
    pub anon_trends: bool,
    pub anon_directory: bool,
    pub anon_directory_federated: bool,
    pub anon_groups: bool,
    pub public_timeline_replies: bool,
    pub boost_collapse: bool,
    pub boost_collapse_lookback: i32,
    pub translation_cache_retention_days: i32,
    pub translation_cache_max_rows: i32,
    pub translation_backend_concurrency: i32,
    pub translation_user_rate_limit_per_hour: i32,
    pub translation_refresh_on_provider_change: bool,
}

/// Replaces every operator-editable field and bumps `updated_at` (the admin
/// settings form always submits the full set, like Mastodon's
/// `Form::AdminSettings`).
#[allow(clippy::too_many_lines, reason = "one flat field-for-field UPDATE")]
pub async fn save(pool: &PgPool, update: SettingsUpdate<'_>) -> Result<InstanceSettings, DbError> {
    let settings = sqlx::query_as!(
        InstanceSettings,
        r#"
        UPDATE instance_settings
        SET site_title = $1,
            site_short_description = $2,
            site_extended_description = $3,
            site_contact_username = $4,
            site_contact_email = $5,
            custom_css = $6,
            registrations_mode = $7,
            rate_limiting_enabled = $8,
            rate_limit_authenticated_api = $9,
            rate_limit_per_token_api = $10,
            rate_limit_unauthenticated_api = $11,
            rate_limit_api_media = $12,
            rate_limit_api_delete = $13,
            rate_limit_api_sign_up = $14,
            rate_limit_app_registrations = $15,
            rate_limit_paging = $16,
            rate_limit_login_attempts = $17,
            rate_limit_password_resets = $18,
            rate_limit_sign_up_web = $19,
            max_characters = $20,
            max_media_attachments = $21,
            poll_max_options = $22,
            activity_api_enabled = $23,
            peers_api_enabled = $24,
            profile_directory = $25,
            trends_enabled = $26,
            trendable_by_default = $27,
            show_domain_blocks = $28,
            show_domain_blocks_rationale = $29,
            media_cache_retention_days = $30,
            min_age = $31,
            media_full_processing = $32,
            media_preview_processing = $33,
            media_remote_full_processing = $34,
            media_cached_image_processing = $35,
            remote_video_max_mb = $36,
            remote_video_max_height = $37,
            group_creation_policy = $38,
            media_local_gif_handling = $39,
            media_remote_gif_handling = $40,
            media_gifv_max_seconds = $41,
            media_avif_quality = $42,
            media_avif_speed_full = $43,
            media_avif_speed_preview = $44,
            media_jpeg_quality = $45,
            media_max_edge = $46,
            media_jxl_distance = $47,
            media_jxl_effort = $48,
            media_video_preset = $49,
            media_video_rate_mode = $50,
            media_video_crf = $51,
            media_audio_bitrate_kbps = $52,
            media_max_image_mb = $53,
            media_max_av_mb = $54,
            media_processing_concurrency = $55,
            media_video_retention_days = $56,
            media_profile_retention_days = $57,
            media_card_retention_days = $58,
            media_emoji_retention_days = $59,
            media_cache_max_gb = $60,
            media_video_cache_max_gb = $61,
            authorized_fetch = $62,
            authorized_fetch_unsigned_profile = $63,
            emit_integrity_proofs = $64,
            emit_rfc9421 = $65,
            conversation_containers = $66,
            ip_retention_days = $67,
            landing_page = $68,
            landing_show_stats = $69,
            timeline_preview_federated = $70,
            timeline_preview_local = $71,
            timeline_preview_tag = $72,
            public_search = $73,
            anon_trends = $74,
            anon_directory = $75,
            anon_directory_federated = $76,
            anon_groups = $77,
            translation_cache_retention_days = $78,
            translation_cache_max_rows = $79,
            translation_backend_concurrency = $80,
            translation_user_rate_limit_per_hour = $81,
            translation_refresh_on_provider_change = $82,
            max_characters_long_form = $83,
            public_timeline_replies = $84,
            boost_collapse = $85,
            boost_collapse_lookback = $86,
            registrations_auto_closed_at = NULL,
            updated_at = now()
        RETURNING site_title, site_short_description, site_extended_description,
                  site_contact_username, site_contact_email, custom_css,
                  registrations_mode, rate_limiting_enabled,
                  rate_limit_authenticated_api, rate_limit_per_token_api,
                  rate_limit_unauthenticated_api, rate_limit_api_media,
                  rate_limit_api_delete, rate_limit_api_sign_up,
                  rate_limit_app_registrations, rate_limit_paging,
                  rate_limit_login_attempts, rate_limit_password_resets,
                  rate_limit_sign_up_web, max_characters, max_characters_long_form,
                  max_media_attachments,
                  poll_max_options, activity_api_enabled, peers_api_enabled,
                  profile_directory, trends_enabled, trendable_by_default,
                  landing_page, landing_show_stats,
                  show_domain_blocks, show_domain_blocks_rationale, min_age,
                  media_cache_retention_days,
                  media_full_processing, media_preview_processing,
                  media_remote_full_processing, media_cached_image_processing,
                  remote_video_max_mb, remote_video_max_height,
                  media_local_gif_handling, media_remote_gif_handling,
                  media_gifv_max_seconds, media_avif_quality,
                  media_avif_speed_full, media_avif_speed_preview,
                  media_jpeg_quality, media_max_edge,
                  media_jxl_distance, media_jxl_effort,
                  media_video_preset, media_video_rate_mode, media_video_crf,
                  media_audio_bitrate_kbps, media_max_image_mb, media_max_av_mb,
                  media_processing_concurrency, media_video_retention_days,
                  media_profile_retention_days, media_card_retention_days,
                  media_emoji_retention_days, media_cache_max_gb,
                  media_video_cache_max_gb,
                  group_creation_policy,
                  authorized_fetch, authorized_fetch_unsigned_profile,
                  emit_integrity_proofs, emit_rfc9421, conversation_containers,
                  ip_retention_days,
                  timeline_preview_federated, timeline_preview_local,
                  timeline_preview_tag, public_search,
                  anon_trends, anon_directory, anon_directory_federated,
                  anon_groups, public_timeline_replies,
                  boost_collapse, boost_collapse_lookback,
                  translation_cache_retention_days, translation_cache_max_rows,
                  translation_backend_concurrency,
                  translation_user_rate_limit_per_hour,
                  translation_refresh_on_provider_change,
                  self_destruct_initiated_at,
                  registrations_auto_closed_at, updated_at
        "#,
        update.site_title,
        update.site_short_description,
        update.site_extended_description,
        update.site_contact_username,
        update.site_contact_email,
        update.custom_css,
        update.registrations_mode.as_str(),
        update.rate_limiting_enabled,
        update.rate_limit_authenticated_api,
        update.rate_limit_per_token_api,
        update.rate_limit_unauthenticated_api,
        update.rate_limit_api_media,
        update.rate_limit_api_delete,
        update.rate_limit_api_sign_up,
        update.rate_limit_app_registrations,
        update.rate_limit_paging,
        update.rate_limit_login_attempts,
        update.rate_limit_password_resets,
        update.rate_limit_sign_up_web,
        update.max_characters,
        update.max_media_attachments,
        update.poll_max_options,
        update.activity_api_enabled,
        update.peers_api_enabled,
        update.profile_directory,
        update.trends_enabled,
        update.trendable_by_default,
        update.show_domain_blocks.as_str(),
        update.show_domain_blocks_rationale.as_str(),
        update.media_cache_retention_days,
        update.min_age,
        update.media_full_processing,
        update.media_preview_processing,
        update.media_remote_full_processing,
        update.media_cached_image_processing,
        update.remote_video_max_mb,
        update.remote_video_max_height,
        update.group_creation_policy.as_str(),
        update.media_local_gif_handling,
        update.media_remote_gif_handling,
        update.media_gifv_max_seconds,
        update.media_avif_quality,
        update.media_avif_speed_full,
        update.media_avif_speed_preview,
        update.media_jpeg_quality,
        update.media_max_edge,
        update.media_jxl_distance,
        update.media_jxl_effort,
        update.media_video_preset,
        update.media_video_rate_mode,
        update.media_video_crf,
        update.media_audio_bitrate_kbps,
        update.media_max_image_mb,
        update.media_max_av_mb,
        update.media_processing_concurrency,
        update.media_video_retention_days,
        update.media_profile_retention_days,
        update.media_card_retention_days,
        update.media_emoji_retention_days,
        update.media_cache_max_gb,
        update.media_video_cache_max_gb,
        update.authorized_fetch,
        update.authorized_fetch_unsigned_profile,
        update.emit_integrity_proofs,
        update.emit_rfc9421,
        update.conversation_containers,
        update.ip_retention_days,
        update.landing_page,
        update.landing_show_stats,
        update.timeline_preview_federated,
        update.timeline_preview_local,
        update.timeline_preview_tag,
        update.public_search,
        update.anon_trends,
        update.anon_directory,
        update.anon_directory_federated,
        update.anon_groups,
        update.translation_cache_retention_days,
        update.translation_cache_max_rows,
        update.translation_backend_concurrency,
        update.translation_user_rate_limit_per_hour,
        update.translation_refresh_on_provider_change,
        // Below here, binds are in the order their columns were *added*, not the
        // order the struct declares: the parameter numbers in the SET clause are
        // positional, so a new field appends here rather than renumbering every
        // existing bind.
        update.max_characters_long_form,
        update.public_timeline_replies,
        update.boost_collapse,
        update.boost_collapse_lookback,
    )
    .fetch_one(pool)
    .await?;
    Ok(settings)
}

/// Stamps the auto-close-registrations notice (O3): the maintenance worker
/// just reverted open registration to approval mode. Cleared by [`save`].
pub async fn mark_registrations_auto_closed(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query!("UPDATE instance_settings SET registrations_auto_closed_at = now()",)
        .execute(pool)
        .await?;
    Ok(())
}

/// Flips the server into self-destruct mode. Returns `false` when it was
/// already set (the timestamp is kept — the mode is one-way).
pub async fn begin_self_destruct(pool: &PgPool) -> Result<bool, DbError> {
    let updated = sqlx::query!(
        r#"
        UPDATE instance_settings
        SET self_destruct_initiated_at = now(), updated_at = now()
        WHERE self_destruct_initiated_at IS NULL
        "#,
    )
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn settings_seeded_and_saved(pool: PgPool) {
        let settings = get(&pool).await.unwrap();
        assert_eq!(settings.site_title, "Plamenu");
        assert_eq!(settings.site_short_description, "");
        assert_eq!(settings.registrations_mode(), RegistrationsMode::None);
        assert!(settings.rate_limiting_enabled);
        assert_eq!(settings.rate_limit_authenticated_api, 1500);
        assert_eq!(settings.rate_limit_per_token_api, 300);
        assert_eq!(settings.max_characters, 5000);
        assert_eq!(settings.max_characters_long_form, 50_000);
        assert_eq!(settings.max_media_attachments, 6);
        assert_eq!(settings.poll_max_options, 6);
        assert!(settings.activity_api_enabled);
        assert!(settings.peers_api_enabled);
        assert!(settings.profile_directory);
        assert!(settings.trends_enabled);
        assert!(!settings.trendable_by_default);
        assert_eq!(settings.media_cache_retention_days, 14);
        // New instances pass image uploads through (metadata stripped) and make
        // AVIF previews; incoming emoji/avatar/card caches re-encode to AVIF.
        assert_eq!(settings.media_full_processing, "passthrough");
        assert_eq!(settings.media_preview_processing, "avif");
        assert_eq!(settings.media_remote_full_processing, "avif");
        assert_eq!(settings.media_cached_image_processing, "avif");
        // Defaults: cache remote videos up to 2 GiB, prefer ≤1080p.
        assert_eq!(settings.remote_video_max_mb, 2048);
        assert_eq!(settings.remote_video_max_height, 1080);
        // Local GIFs become gifv (alpha ones stay GIF); remote GIFs become WebP.
        assert_eq!(settings.media_local_gif_handling, "gifv");
        assert_eq!(settings.media_remote_gif_handling, "webp");
        assert_eq!(settings.media_gifv_max_seconds, 60);
        assert_eq!(settings.media_avif_quality, 70);
        assert_eq!(settings.media_avif_speed_full, 10);
        assert_eq!(settings.media_avif_speed_preview, 6);
        assert_eq!(settings.media_jpeg_quality, 85);
        assert_eq!(settings.media_max_edge, 1920);
        assert!((settings.media_jxl_distance - 1.0).abs() < f32::EPSILON);
        assert_eq!(settings.media_jxl_effort, 4);
        assert_eq!(settings.media_video_preset, "veryfast");
        assert_eq!(settings.media_video_rate_mode, "abr");
        assert_eq!(settings.media_video_crf, 23);
        assert_eq!(settings.media_audio_bitrate_kbps, 192);
        assert_eq!(settings.media_max_image_mb, 16);
        assert_eq!(settings.media_max_av_mb, 99);
        assert_eq!(settings.media_processing_concurrency, 0);
        assert_eq!(settings.media_video_retention_days, None);
        assert_eq!(settings.media_profile_retention_days, 0);
        assert_eq!(settings.media_card_retention_days, 0);
        assert_eq!(settings.media_emoji_retention_days, 0);
        assert_eq!(settings.media_cache_max_gb, 0);
        assert_eq!(settings.media_video_cache_max_gb, 0);
        assert_eq!(
            settings.show_domain_blocks(),
            DomainBlocksDisclosure::Disabled
        );
        assert_eq!(
            settings.show_domain_blocks_rationale(),
            DomainBlocksDisclosure::Disabled
        );
        // Default: anyone may create groups.
        assert_eq!(
            settings.group_creation_policy(),
            GroupCreationPolicy::Everyone
        );
        // O4 federation overrides ship unset (follow the config file); the
        // settings-owned knobs ship at their production defaults.
        assert_eq!(settings.authorized_fetch, None);
        assert_eq!(settings.authorized_fetch_unsigned_profile, None);
        assert!(settings.emit_integrity_proofs);
        assert!(settings.emit_rfc9421);
        assert_eq!(settings.conversation_containers, None);
        assert_eq!(settings.ip_retention_days, 365);
        // Anonymous access ships private-by-default.
        assert!(!settings.timeline_preview_federated);
        assert!(!settings.timeline_preview_local);
        assert!(!settings.timeline_preview_tag);
        assert!(!settings.public_search);
        // The shared timelines ship Mastodon-shaped — no replies.
        assert!(!settings.public_timeline_replies);
        // Collapse on, but page-local — the lookback window is the
        // part that costs a second timeline query, so it ships off.
        assert!(settings.boost_collapse);
        assert_eq!(settings.boost_collapse_lookback, 0);

        let saved = save(
            &pool,
            SettingsUpdate {
                site_title: "Testburg",
                site_short_description: "A test instance",
                site_extended_description: "## Long form\n\nWelcome.",
                site_contact_username: "admin",
                site_contact_email: "admin@example.com",
                custom_css: ".column { color: red }",
                registrations_mode: RegistrationsMode::Approved,
                rate_limiting_enabled: false,
                rate_limit_authenticated_api: 2000,
                rate_limit_per_token_api: 400,
                rate_limit_unauthenticated_api: 100,
                rate_limit_api_media: 10,
                rate_limit_api_delete: 20,
                rate_limit_api_sign_up: 3,
                rate_limit_app_registrations: 7,
                rate_limit_paging: 200,
                rate_limit_login_attempts: 30,
                rate_limit_password_resets: 4,
                rate_limit_sign_up_web: 15,
                max_characters: 5000,
                max_characters_long_form: 120_000,
                max_media_attachments: 6,
                poll_max_options: 8,
                activity_api_enabled: false,
                peers_api_enabled: false,
                profile_directory: false,
                landing_page: false,
                landing_show_stats: false,
                trends_enabled: false,
                trendable_by_default: true,
                show_domain_blocks: DomainBlocksDisclosure::All,
                show_domain_blocks_rationale: DomainBlocksDisclosure::Users,
                min_age: 16,
                media_cache_retention_days: 21,
                media_full_processing: "avif",
                media_preview_processing: "jpeg",
                media_remote_full_processing: "avif",
                media_cached_image_processing: "passthrough",
                remote_video_max_mb: 512,
                remote_video_max_height: 480,
                media_local_gif_handling: "keep",
                media_remote_gif_handling: "webp",
                media_gifv_max_seconds: 30,
                media_avif_quality: 70,
                media_avif_speed_full: 8,
                media_avif_speed_preview: 4,
                media_jpeg_quality: 90,
                media_max_edge: 2560,
                media_jxl_distance: 2.0,
                media_jxl_effort: 7,
                media_video_preset: "faster",
                media_video_rate_mode: "crf",
                media_video_crf: 21,
                media_audio_bitrate_kbps: 128,
                media_max_image_mb: 8,
                media_max_av_mb: 50,
                media_processing_concurrency: 3,
                media_video_retention_days: Some(7),
                media_profile_retention_days: 30,
                media_card_retention_days: 14,
                media_emoji_retention_days: 90,
                media_cache_max_gb: 20,
                media_video_cache_max_gb: 50,
                group_creation_policy: GroupCreationPolicy::Admins,
                authorized_fetch: Some(false),
                authorized_fetch_unsigned_profile: Some(true),
                emit_integrity_proofs: false,
                emit_rfc9421: false,
                conversation_containers: Some(true),
                ip_retention_days: 30,
                timeline_preview_federated: true,
                timeline_preview_local: true,
                timeline_preview_tag: false,
                public_search: true,
                anon_trends: false,
                anon_directory: false,
                anon_directory_federated: true,
                anon_groups: false,
                public_timeline_replies: true,
                boost_collapse: false,
                boost_collapse_lookback: 80,
                translation_cache_retention_days: 14,
                translation_cache_max_rows: 1000,
                translation_backend_concurrency: 4,
                translation_user_rate_limit_per_hour: 10,
                translation_refresh_on_provider_change: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(saved.site_title, "Testburg");
        assert_eq!(saved.min_age, 16);
        assert_eq!(saved.registrations_mode(), RegistrationsMode::Approved);
        assert!(!saved.rate_limiting_enabled);
        assert_eq!(saved.rate_limit_authenticated_api, 2000);
        assert_eq!(saved.rate_limit_sign_up_web, 15);
        assert_eq!(saved.max_characters, 5000);
        assert_eq!(saved.max_characters_long_form, 120_000);
        assert_eq!(saved.max_media_attachments, 6);
        assert_eq!(saved.poll_max_options, 8);
        assert!(!saved.activity_api_enabled);
        assert!(!saved.peers_api_enabled);
        assert!(!saved.profile_directory);
        assert!(!saved.trends_enabled);
        assert!(saved.trendable_by_default);
        assert_eq!(saved.media_cache_retention_days, 21);
        assert_eq!(saved.media_full_processing, "avif");
        assert_eq!(saved.group_creation_policy(), GroupCreationPolicy::Admins);
        assert_eq!(saved.media_preview_processing, "jpeg");
        assert_eq!(saved.media_remote_full_processing, "avif");
        assert_eq!(saved.media_cached_image_processing, "passthrough");
        assert_eq!(saved.remote_video_max_mb, 512);
        assert_eq!(saved.remote_video_max_height, 480);
        assert_eq!(saved.media_local_gif_handling, "keep");
        assert_eq!(saved.media_remote_gif_handling, "webp");
        assert_eq!(saved.media_gifv_max_seconds, 30);
        assert_eq!(saved.media_avif_quality, 70);
        assert_eq!(saved.media_video_rate_mode, "crf");
        assert_eq!(saved.media_max_av_mb, 50);
        assert_eq!(saved.media_video_retention_days, Some(7));
        assert_eq!(saved.media_cache_max_gb, 20);
        assert_eq!(saved.show_domain_blocks(), DomainBlocksDisclosure::All);
        assert_eq!(
            saved.show_domain_blocks_rationale(),
            DomainBlocksDisclosure::Users
        );
        assert_eq!(saved.authorized_fetch, Some(false));
        assert_eq!(saved.authorized_fetch_unsigned_profile, Some(true));
        assert!(!saved.emit_integrity_proofs);
        assert!(!saved.emit_rfc9421);
        assert_eq!(saved.conversation_containers, Some(true));
        assert_eq!(saved.ip_retention_days, 30);
        assert!(saved.timeline_preview_federated);
        assert!(saved.timeline_preview_local);
        assert!(!saved.timeline_preview_tag);
        assert!(saved.public_search);
        assert!(!saved.anon_trends);
        assert!(!saved.anon_directory);
        assert!(saved.anon_directory_federated);
        assert!(!saved.anon_groups);
        assert!(saved.public_timeline_replies);
        assert!(!saved.boost_collapse);
        assert_eq!(saved.boost_collapse_lookback, 80);
        assert!(saved.updated_at >= settings.updated_at);

        let round_trip = get(&pool).await.unwrap();
        assert_eq!(round_trip.site_contact_email, "admin@example.com");
        assert_eq!(round_trip.custom_css, ".column { color: red }");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn self_destruct_is_one_way_and_survives_saves(pool: PgPool) {
        let settings = get(&pool).await.unwrap();
        assert!(!settings.is_self_destructing());

        assert!(begin_self_destruct(&pool).await.unwrap());
        let armed = get(&pool).await.unwrap();
        assert!(armed.is_self_destructing());

        // A repeat initiation is a no-op that keeps the original timestamp.
        assert!(!begin_self_destruct(&pool).await.unwrap());
        let repeat = get(&pool).await.unwrap();
        assert_eq!(
            repeat.self_destruct_initiated_at,
            armed.self_destruct_initiated_at
        );

        // The admin settings form's full-set save must not clear the flag.
        save(&pool, armed.as_update()).await.unwrap();
        assert!(get(&pool).await.unwrap().is_self_destructing());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn registrations_auto_close_stamp_cleared_by_save(pool: PgPool) {
        assert_eq!(get(&pool).await.unwrap().registrations_auto_closed_at, None);

        mark_registrations_auto_closed(&pool).await.unwrap();
        let stamped = get(&pool).await.unwrap();
        assert!(stamped.registrations_auto_closed_at.is_some());

        // Any operator save acknowledges (clears) the notice.
        let saved = save(&pool, stamped.as_update()).await.unwrap();
        assert_eq!(saved.registrations_auto_closed_at, None);
    }
}
