//! TOML server configuration and documented config-file generation.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroize;

/// Configuration secret whose routine `Debug` output is always redacted.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

const DEFAULT_BIND: &str = "127.0.0.1:8420";
const DEFAULT_MEDIA_DIR: &str = "./media";

/// Default size of the shared `PostgreSQL` connection pool. Suits a single-node
/// deployment; a busy instance can raise `database_pool_size`.
const DEFAULT_DB_POOL_SIZE: u32 = 16;

/// Sanity ceiling on the configured pool size — well above any single-node
/// need, but low enough that a fat-fingered value cannot try to open thousands
/// of connections and exhaust `PostgreSQL`'s own `max_connections`.
const MAX_DB_POOL_SIZE: u32 = 1024;

/// A complete, documented config with required values left for the operator.
/// Settings with built-in defaults are commented out so upgrades can change
/// those defaults until the operator deliberately pins a value.
pub const CONFIG_TEMPLATE: &str = r#"# Plamenu configuration
#
# Generate a fresh copy with: plamenu config generate
# Required settings are active and empty. Plamenu will not start until all three
# have values. All other settings are commented out so the built-in defaults
# continue to follow the installed Plamenu version.

# Public hostname only, without a scheme or path. This is where Plamenu's web,
# API, and ActivityPub URLs are served. It becomes part of permanent object IDs
# and must not change after federation starts.
domain = ""

# Optional shorter domain for account handles. For example, with
# `domain = "social.example.com"` and `account_domain = "example.com"`, Alice's
# handle is `alice@example.com` while her actor and profile remain hosted at
# `https://social.example.com`. The hosting domain must equal this value or be
# one of its subdomains. Configure the account domain's reverse proxy to send
# (or redirect, preserving the query string) `/.well-known/webfinger`,
# `/.well-known/host-meta`, and `/.well-known/nodeinfo` to `domain`. This value
# is also permanent once identities federate. Unset defaults to `domain`.
# account_domain = "example.com"

# Each Webxdc session is served from `<session>.webxdc.<domain>`. Configure
# wildcard DNS and TLS for `*.webxdc.<domain>` and point it at the same Plamenu
# service. These origins deliberately receive no Plamenu login cookie.

# PostgreSQL connection URL.
database_url = ""

# Size of the shared PostgreSQL connection pool that HTTP handlers and all
# background workers draw from. The default suits a single-node deployment;
# raise it for a busy instance, but keep it under your PostgreSQL server's own
# max_connections. Must be between 1 and 1024.
# database_pool_size = 16

# HTTP listen address.
# bind = "127.0.0.1:8420"

# Waive the outbound private-address SSRF guard. Use only in closed development
# or test networks where federated peers intentionally resolve to private IPs.
# allow_private_fetch = false

# Require valid HTTP signatures for server-to-server reads of posts,
# collections, and the social graph.
# authorized_fetch = true

# Under authorized_fetch, actor documents still answer UNSIGNED peers so they
# can fetch your key and verify your deliveries (e.g. default-config Lemmy).
# Serve them the profile document (display name, avatar, fields — best interop)
# or only key and routing data. Signed peers always get the profile document,
# and this is ignored when authorized_fetch is off. Values: "profile", "key-only".
# authorized_fetch_unsigned = "profile"

# Local media storage and external media-tool binaries. Media processing and
# retention behavior is configured live in the admin dashboard.
# media_dir = "./media"
# ffmpeg_path = "ffmpeg"
# Native RTMP ingest uses this listener; loopback only by default. To allow
# an external encoder, bind a reachable address and protect the plaintext
# RTMP hop (for example with a private network or TLS terminator).
# live_ingest_bind = "127.0.0.1:1935"
# ffprobe_path = "ffprobe"

# Native Video uploads use the built-in background worker and local media store.
# Opt-in channel delivery aggregates can be disabled server-wide. This is a
# root-level setting, before any TOML section. No viewer identities are stored.
# video_analytics = true

# Reverse proxies whose X-Forwarded-For values are trusted. Bare IP addresses
# are accepted and converted to host routes. Defaults to loopback only; behind
# a proxy on another address, list that proxy's exact address or subnet — do NOT
# trust broad private ranges, or any host on them can forge client IPs. Use []
# to trust no proxies.
# trusted_proxies = ["127.0.0.0/8", "::1/128"]

# Stable secret used to encrypt federation signing keys and TOTP secrets at
# rest. Required for server startup; example generator: openssl rand -hex 32
# Keep outside
# database backups. Rotate by raising the version, moving the old value to the
# versioned previous list, then running the documented rewrap procedure.
encryption_secret = ""
encryption_secret_version = 1
# encryption_previous_secrets = ["2:replace-with-an-independent-32-byte-random-secret"]



# JSON release feed polled for software-update notices. Unset disables checks.
# update_check_url = "https://example.com/plamenu-releases.json"

# FEP-171b conversation containers: owner-side emission of Add activities for
# private/direct threads. Off by default — few peers consume it yet.
# conversation_containers = false

# Ask browsers to POST Content-Security-Policy violation details to the local
# /csp-report sink. Reports may contain page, source, and blocked-resource URLs,
# so this telemetry is disabled by default. Enable it only for deliberate,
# limited debugging on an instance whose operators accept that collection.
# csp_reporting = false

# Optional paired native disks; leave absent for the normal installation.
# See the native media storage guide before enabling on an existing instance.
# [media_mirror]
# directory = "/srv/plamenu-replica"
# bandwidth_mib_per_second = 32
# min_free_gib = 1
# Optional native, consented federation copies. No companion process needed.
# Exact source/mirror domains must additionally be approved by an operator.
# [video_redundancy]
# quota_gib = 2
# max_mirror_mib = 512
# bandwidth_mib_per_second = 8
# retention_hours = 24

# Optional native RTMPS for remote encoders. Reuse your existing TLS
# certificate and private key; Plamenu reloads them after renewal. No proxy
# or companion media service is needed. Configure all three values together.
# [live_rtmps]
# bind = "0.0.0.0:1936"
# cert = "/etc/letsencrypt/live/social.example.com/fullchain.pem"
# key = "/etc/letsencrypt/live/social.example.com/privkey.pem"

# These limits need no companion service. The ordinary attachment uploader has
# its own limits. A larger source limit increases the processing scratch budget.
# [native_video]
# max_upload_mib = 2048
# per_account_gib = 50
# global_gib = 1024
# pending_gib = 8
# unfinished_drafts = 32
# jobs = 2
# min_free_gib = 1

# Optional private video encoding on another host running the same binary.
# No worker is required for the normal native installation. Generate a private
# worker file with `plamenu --config worker.toml config generate --video-worker`.
# The origin alone keeps its database, source and publication authority.
# [[video_offload]]
# url = "https://processor.example/"
# token = "replace-with-the-private-worker-generated-token"

# Proof-of-work protecting the browser sign-up form. Defaults follow the
# official ALTCHA v2 PBKDF2 example: clients search counters 5,000–10,000 with
# 5,000 PBKDF2 rounds per attempt, and each signed challenge expires in five
# minutes. Lower values reduce sign-up latency but also reduce bot resistance.
# [altcha]
# cost = 5000
# min_counter = 5000
# max_counter = 10000
# expires_seconds = 300

# Outgoing mail is disabled when this whole section is absent. If enabled,
# server is required; the remaining settings have the defaults shown here.
# [smtp]
# server = "smtp.example.com"
# port = 587
# login = "username"
# password = "password"
# from_address = "Plamenu <notifications@example.com>"
# ssl = false
# starttls = "auto" # accepted values: "auto", "always", "never"

# Status translation is disabled when this whole section is absent. Choose one
# backend. DeepL plans are "free" or "pro".
# [translation]
# backend = "deepl"
# api_key = "replace-with-a-deepl-api-key"
# plan = "free"

# LibreTranslate example (use instead of the section above):
# [translation]
# backend = "libretranslate"
# endpoint = "https://translate.example.com"
# api_key = "optional-api-key"
# OpenAI-compatible example (a llama.cpp llama-server hosting a translation
# model such as Hy-MT2; "languages" overrides the built-in Hy-MT2 list):
# [translation]
# backend = "openai"
# endpoint = "http://127.0.0.1:8080"
# model = "hy-mt2-1.8b"
# api_key = "optional-bearer-token"

# Outbound proxy routing. Most deployments never need this section; its main
# use is federating with .onion/.i2p instances through a local Tor/I2P
# daemon's SOCKS proxy. Proxy URLs must be socks5h:// (the `h` makes the proxy
# resolve hostnames — required, since .onion names have no DNS) or http(s)://
# for an HTTP CONNECT proxy. Without an onion/i2p-capable proxy configured,
# deliveries and fetches toward hidden services simply fail. Per-overlay
# proxies leave clearnet federation direct. A global proxy_url also handles
# clearnet and moves DNS destination filtering to that proxy's egress ACL.
# Webhooks and the translation backend always connect directly (they may point
# at internal services).
# [federation]
# Applies to ALL outbound federation traffic (fetches, deliveries, media).
# proxy_url = ""
# Required with a global proxy_url while allow_private_fetch=false. Set only
# after the proxy ACL denies loopback, link-local, private/ULA, metadata-service,
# host-network, and administrative destinations.
# trust_proxy_destination_filtering = false
# Applies only to .onion destinations; falls back to proxy_url.
# onion_proxy_url = "socks5h://127.0.0.1:9050"
# Applies only to .i2p destinations; falls back to proxy_url.
# i2p_proxy_url = "socks5h://127.0.0.1:4447"
# Hosts that bypass every proxy (exact hostname match, no wildcards).
# no_proxy = []
"#;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file {path} could not be read: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    // Do not retain/display toml::de::Error here: its Display output includes
    // the offending source line, which may be database_url, SMTP password, or
    // another secret-bearing setting.
    #[error("configuration file {path} is invalid; check TOML syntax and field types")]
    Parse { path: PathBuf },
    #[error("required configuration field `{0}` is missing or empty")]
    Missing(&'static str),
    #[error("invalid value for configuration field `{0}`: {1}")]
    Invalid(&'static str, String),
    #[error("configuration file {0} already exists (use --force to replace it)")]
    AlreadyExists(PathBuf),
    #[error("configuration file {path} could not be written: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// HTTPS/API/ActivityPub hosting domain; the origin of permanent URLs.
    pub domain: String,
    /// Domain used in canonical `acct:user@domain` handles.
    pub account_domain: String,
    pub bind: SocketAddr,
    pub database_url: String,
    /// Size of the shared `PostgreSQL` connection pool (`database_pool_size`).
    pub db_pool_size: u32,
    pub allow_private_fetch: bool,
    pub authorized_fetch: bool,
    pub authorized_fetch_unsigned_profile: bool,
    pub media_dir: PathBuf,
    /// Optional second native disk; absent means the original single-disk install.
    pub media_mirror: Option<MediaMirrorConfig>,
    pub video_redundancy: Option<VideoRedundancyConfig>,
    /// Operator kill switch. Channels still opt in individually; no tracking IDs.
    pub video_analytics: bool,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    /// RTMP publisher listener (loopback by default because RTMP is plaintext).
    pub live_ingest_bind: SocketAddr,
    pub live_rtmps: Option<LiveRtmpsConfig>,
    pub native_video: NativeVideoConfig,
    /// Optional private processors; the origin alone owns DB/publication rights.
    pub video_offload: Vec<crate::video_offload::Target>,
    pub smtp: Option<SmtpConfig>,
    pub trusted_proxies: Vec<String>,
    pub encryption_secret: Option<SecretString>,
    /// Stable version embedded in federation-key ciphertext envelopes.
    pub encryption_secret_version: i32,
    /// Versioned decrypt-only keys used during rolling at-rest key rotation.
    pub encryption_previous_secrets: Vec<(i32, SecretString)>,
    pub altcha: AltchaConfig,
    pub update_check_url: Option<String>,
    pub translation: Option<TranslationConfig>,
    pub conversation_containers: bool,
    /// Whether HTML CSPs advertise, and the router exposes, `/csp-report`.
    pub csp_reporting: bool,
    pub federation: FederationConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaMirrorConfig {
    pub directory: PathBuf,
    #[serde(default = "mirror_bandwidth")]
    pub bandwidth_mib_per_second: u64,
    #[serde(default = "mirror_free_floor")]
    pub min_free_gib: u64,
}

/// Optional federation mirrors: no extra process or installation dependency.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoRedundancyConfig {
    pub quota_gib: u64,
    pub max_mirror_mib: u64,
    pub bandwidth_mib_per_second: u64,
    pub retention_hours: u64,
}
impl Default for VideoRedundancyConfig {
    fn default() -> Self {
        Self {
            quota_gib: 2,
            max_mirror_mib: 512,
            bandwidth_mib_per_second: 8,
            retention_hours: 24,
        }
    }
}
impl VideoRedundancyConfig {
    fn validate(self) -> Result<Self, ConfigError> {
        if !(1..=1024).contains(&self.quota_gib)
            || !(1..=4096).contains(&self.max_mirror_mib)
            || !(1..=1024).contains(&self.bandwidth_mib_per_second)
            || !(1..=168).contains(&self.retention_hours)
            || self.max_mirror_mib > self.quota_gib * 1024
            || self.max_mirror_mib.div_ceil(self.bandwidth_mib_per_second) > 540
        {
            return Err(ConfigError::Invalid("video_redundancy", "requires bounded positive quota, copy size, bandwidth and retention; each copy must fit its finite deadline".into()));
        }
        Ok(self)
    }
}
const fn mirror_bandwidth() -> u64 {
    32
}
const fn mirror_free_floor() -> u64 {
    1
}

/// Work and lifetime limits for browser sign-up proof-of-work challenges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltchaConfig {
    pub cost: u32,
    pub min_counter: u32,
    pub max_counter: u32,
    pub expires_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct LiveRtmpsConfig {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Native Video admission and worker budgets, independent of ordinary media.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeVideoConfig {
    pub max_upload_mib: u64,
    pub per_account_gib: u64,
    pub global_gib: u64,
    pub pending_gib: u64,
    pub unfinished_drafts: u32,
    pub jobs: usize,
    pub min_free_gib: u64,
}

impl Default for NativeVideoConfig {
    fn default() -> Self {
        Self {
            max_upload_mib: 2048,
            per_account_gib: 50,
            global_gib: 1024,
            pending_gib: 8,
            unfinished_drafts: 32,
            jobs: 2,
            min_free_gib: 1,
        }
    }
}

impl NativeVideoConfig {
    pub const MIB: u64 = 1024 * 1024;
    pub const GIB: u64 = 1024 * Self::MIB;

    #[must_use]
    pub fn max_upload_bytes(self) -> u64 {
        self.max_upload_mib * Self::MIB
    }

    /// A small source still gets enough room for a high-quality re-encode;
    /// large sources cannot make an output exceed the upload ceiling.
    #[must_use]
    pub fn output_limit(self, source_bytes: u64) -> u64 {
        self.max_upload_bytes()
            .min(source_bytes.saturating_mul(4).max(256 * Self::MIB))
    }

    #[must_use]
    pub fn processing_budget(self) -> u64 {
        let file = self.max_upload_bytes();
        // Compatibility MP4 plus at most six single-file HLS renditions;
        // count each scratch output and a possible cross-filesystem copy.
        file + 2 * (1 + crate::media_transcode::NATIVE_HLS_MAX_RENDITIONS) * file
            + 3 * crate::media_transcode::POSTER_FILE_LIMIT_BYTES
            + 2 * crate::media_transcode::STORYBOARD_FILE_LIMIT_BYTES
    }

    #[must_use]
    pub fn storage_limits(self) -> plamenu_db::video_storage::Limits {
        plamenu_db::video_storage::Limits {
            per_account: i64::try_from(self.per_account_gib * Self::GIB).unwrap_or(i64::MAX),
            global: i64::try_from(self.global_gib * Self::GIB).unwrap_or(i64::MAX),
        }
    }

    pub(crate) fn validate(self) -> Result<Self, ConfigError> {
        if !(1..=8192).contains(&self.max_upload_mib)
            || !(1..=65_536).contains(&self.per_account_gib)
            || !(1..=65_536).contains(&self.global_gib)
            || self.per_account_gib > self.global_gib
            || !(1..=1024).contains(&self.pending_gib)
            || self.pending_gib > self.global_gib
            || !(1..=128).contains(&self.unfinished_drafts)
            || !(1..=4).contains(&self.jobs)
            || !(1..=64).contains(&self.min_free_gib)
        {
            return Err(ConfigError::Invalid(
                "native_video",
                "limits must be positive, within documented bounds, and fit the global budget"
                    .to_owned(),
            ));
        }
        Ok(self)
    }
}

impl Default for AltchaConfig {
    fn default() -> Self {
        Self {
            cost: 5_000,
            min_counter: 5_000,
            max_counter: 10_000,
            expires_seconds: 300,
        }
    }
}

/// Outbound proxy routing (`[federation]`). Infrastructure settings in the
/// same family as `database_url` and `[smtp]`, deliberately not live admin
/// settings: a proxy change requires rebuilding the outbound HTTP clients,
/// which only happens at startup.
#[derive(Clone, Default)]
pub struct FederationConfig {
    /// Routes all outbound federation traffic.
    pub proxy_url: Option<String>,
    /// Routes `.onion` destinations only; falls back to `proxy_url`.
    pub onion_proxy_url: Option<String>,
    /// Routes `.i2p` destinations only; falls back to `proxy_url`.
    pub i2p_proxy_url: Option<String>,
    /// Explicit acknowledgement that a global proxy, not Plamenu's resolver,
    /// is authoritative for clearnet destination filtering.
    pub trust_proxy_destination_filtering: bool,
    /// Hosts that bypass every proxy (exact hostname match).
    pub no_proxy: Vec<String>,
}

#[derive(Clone)]
pub enum TranslationConfig {
    DeepL {
        plan: DeepLPlan,
        api_key: String,
    },
    LibreTranslate {
        endpoint: String,
        api_key: Option<String>,
    },
    /// An OpenAI-compatible chat-completions server hosting a translation
    /// model — in practice llama.cpp's `llama-server` running Hy-MT2.
    OpenAi {
        /// Base URL, e.g. `http://10.0.0.5:8080`; `/v1/chat/completions` is
        /// appended.
        endpoint: String,
        /// Sent as the `model` field and shown as the attribution provider.
        model: String,
        /// Optional bearer token.
        api_key: Option<String>,
        /// Language codes the model translates between (any → any); defaults
        /// to Hy-MT2's 33 supported languages.
        languages: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepLPlan {
    Free,
    Pro,
}

#[derive(Clone)]
pub struct SmtpConfig {
    pub server: String,
    pub port: u16,
    pub login: Option<String>,
    pub password: Option<String>,
    pub from_address: String,
    pub ssl: bool,
    pub starttls: Starttls,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("domain", &self.domain)
            .field("account_domain", &self.account_domain)
            .field("bind", &self.bind)
            .field("database_url", &"[REDACTED]")
            .field("db_pool_size", &self.db_pool_size)
            .field("allow_private_fetch", &self.allow_private_fetch)
            .field("authorized_fetch", &self.authorized_fetch)
            .field(
                "authorized_fetch_unsigned_profile",
                &self.authorized_fetch_unsigned_profile,
            )
            .field("media_dir", &self.media_dir)
            .field("media_mirror", &self.media_mirror)
            .field("video_redundancy", &self.video_redundancy)
            .field("video_analytics", &self.video_analytics)
            .field("native_video", &self.native_video)
            .field("video_offload", &self.video_offload)
            .field("ffmpeg_path", &self.ffmpeg_path)
            .field("ffprobe_path", &self.ffprobe_path)
            .field("live_ingest_bind", &self.live_ingest_bind)
            .field("live_rtmps", &self.live_rtmps)
            .field("smtp", &self.smtp)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("encryption_secret", &"[REDACTED]")
            .field(
                "encryption_previous_secret_versions",
                &self
                    .encryption_previous_secrets
                    .iter()
                    .map(|(version, _)| *version)
                    .collect::<Vec<_>>(),
            )
            .field("encryption_secret_version", &self.encryption_secret_version)
            .field("altcha", &self.altcha)
            .field(
                "update_check_url_configured",
                &self.update_check_url.is_some(),
            )
            .field("translation", &self.translation)
            .field("conversation_containers", &self.conversation_containers)
            .field("csp_reporting", &self.csp_reporting)
            .field("federation", &self.federation)
            .finish()
    }
}

impl std::fmt::Debug for FederationConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FederationConfig")
            .field("proxy_url_configured", &self.proxy_url.is_some())
            .field(
                "onion_proxy_url_configured",
                &self.onion_proxy_url.is_some(),
            )
            .field("i2p_proxy_url_configured", &self.i2p_proxy_url.is_some())
            .field(
                "trust_proxy_destination_filtering",
                &self.trust_proxy_destination_filtering,
            )
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

impl std::fmt::Debug for TranslationConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeepL { plan, .. } => formatter
                .debug_struct("DeepL")
                .field("plan", plan)
                .field("api_key", &"[REDACTED]")
                .finish(),
            Self::LibreTranslate { api_key, .. } => formatter
                .debug_struct("LibreTranslate")
                .field("endpoint_configured", &true)
                .field("api_key_configured", &api_key.is_some())
                .finish(),
            Self::OpenAi {
                model,
                api_key,
                languages,
                ..
            } => formatter
                .debug_struct("OpenAi")
                .field("endpoint_configured", &true)
                .field("model", model)
                .field("api_key_configured", &api_key.is_some())
                .field("languages", languages)
                .finish(),
        }
    }
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SmtpConfig")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("login_configured", &self.login.is_some())
            .field("password", &"[REDACTED]")
            .field("from_address", &self.from_address)
            .field("ssl", &self.ssl)
            .field("starttls", &self.starttls)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Starttls {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    domain: Option<String>,
    account_domain: Option<String>,
    database_url: Option<String>,
    database_pool_size: Option<u32>,
    bind: Option<SocketAddr>,
    allow_private_fetch: Option<bool>,
    authorized_fetch: Option<bool>,
    authorized_fetch_unsigned: Option<AuthorizedFetchUnsigned>,
    media_dir: Option<PathBuf>,
    media_mirror: Option<MediaMirrorConfig>,
    video_redundancy: Option<VideoRedundancyConfig>,
    video_analytics: Option<bool>,
    ffmpeg_path: Option<String>,
    ffprobe_path: Option<String>,
    live_ingest_bind: Option<SocketAddr>,
    live_rtmps: Option<LiveRtmpsConfigFile>,
    native_video: Option<FileNativeVideoConfig>,
    video_offload: Option<Vec<crate::video_offload::Target>>,
    trusted_proxies: Option<Vec<String>>,
    encryption_secret: Option<String>,
    encryption_secret_version: Option<i32>,
    encryption_previous_secrets: Option<Vec<String>>,
    altcha: Option<FileAltchaConfig>,
    update_check_url: Option<String>,
    conversation_containers: Option<bool>,
    csp_reporting: Option<bool>,
    smtp: Option<FileSmtpConfig>,
    translation: Option<FileTranslationConfig>,
    federation: Option<FileFederationConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveRtmpsConfigFile {
    bind: Option<SocketAddr>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAltchaConfig {
    cost: Option<u32>,
    min_counter: Option<u32>,
    max_counter: Option<u32>,
    expires_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileNativeVideoConfig {
    max_upload_mib: Option<u64>,
    per_account_gib: Option<u64>,
    global_gib: Option<u64>,
    pending_gib: Option<u64>,
    unfinished_drafts: Option<u32>,
    jobs: Option<usize>,
    min_free_gib: Option<u64>,
}

impl FileNativeVideoConfig {
    fn build(self) -> Result<NativeVideoConfig, ConfigError> {
        let defaults = NativeVideoConfig::default();
        NativeVideoConfig {
            max_upload_mib: self.max_upload_mib.unwrap_or(defaults.max_upload_mib),
            per_account_gib: self.per_account_gib.unwrap_or(defaults.per_account_gib),
            global_gib: self.global_gib.unwrap_or(defaults.global_gib),
            pending_gib: self.pending_gib.unwrap_or(defaults.pending_gib),
            unfinished_drafts: self.unfinished_drafts.unwrap_or(defaults.unfinished_drafts),
            jobs: self.jobs.unwrap_or(defaults.jobs),
            min_free_gib: self.min_free_gib.unwrap_or(defaults.min_free_gib),
        }
        .validate()
    }
}

impl FileAltchaConfig {
    fn build(self) -> Result<AltchaConfig, ConfigError> {
        let defaults = AltchaConfig::default();
        let config = AltchaConfig {
            cost: self.cost.unwrap_or(defaults.cost),
            min_counter: self.min_counter.unwrap_or(defaults.min_counter),
            max_counter: self.max_counter.unwrap_or(defaults.max_counter),
            expires_seconds: self.expires_seconds.unwrap_or(defaults.expires_seconds),
        };
        if config.cost == 0 || config.cost > 100_000 {
            return Err(ConfigError::Invalid(
                "altcha.cost",
                "must be between 1 and 100000".to_owned(),
            ));
        }
        if config.min_counter == 0
            || config.max_counter < config.min_counter
            || config.max_counter > 1_000_000
        {
            return Err(ConfigError::Invalid(
                "altcha.min_counter",
                "counter range must be ordered, non-zero, and no higher than 1000000".to_owned(),
            ));
        }
        if !(60..=3_600).contains(&config.expires_seconds) {
            return Err(ConfigError::Invalid(
                "altcha.expires_seconds",
                "must be between 60 and 3600".to_owned(),
            ));
        }
        Ok(config)
    }
}

fn build_altcha_config(config: Option<FileAltchaConfig>) -> Result<AltchaConfig, ConfigError> {
    config
        .map(FileAltchaConfig::build)
        .transpose()
        .map(Option::unwrap_or_default)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFederationConfig {
    proxy_url: Option<String>,
    onion_proxy_url: Option<String>,
    i2p_proxy_url: Option<String>,
    trust_proxy_destination_filtering: Option<bool>,
    no_proxy: Option<Vec<String>>,
}

impl FileFederationConfig {
    fn build(self) -> Result<FederationConfig, ConfigError> {
        Ok(FederationConfig {
            proxy_url: validate_proxy_url("federation.proxy_url", self.proxy_url)?,
            onion_proxy_url: validate_proxy_url(
                "federation.onion_proxy_url",
                self.onion_proxy_url,
            )?,
            i2p_proxy_url: validate_proxy_url("federation.i2p_proxy_url", self.i2p_proxy_url)?,
            trust_proxy_destination_filtering: self
                .trust_proxy_destination_filtering
                .unwrap_or(false),
            no_proxy: self
                .no_proxy
                .unwrap_or_default()
                .iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
        })
    }
}

/// Validates a `[federation]` proxy URL at load time. The one footgun closed
/// here: `socks5://` resolves hostnames *locally*, so it can never reach a
/// `.onion`/`.i2p` destination — only `socks5h://` (proxy-side resolution)
/// works, and the error says so instead of leaving the operator with silent
/// resolution failures at runtime.
fn validate_proxy_url(
    key: &'static str,
    value: Option<String>,
) -> Result<Option<String>, ConfigError> {
    let Some(value) = nonempty(value) else {
        return Ok(None);
    };
    let url: url::Url = value
        .parse()
        .map_err(|_| ConfigError::Invalid(key, "must be a valid proxy URL".to_owned()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::Invalid(
            key,
            "proxy URL userinfo is not supported; configure authentication at the proxy boundary"
                .to_owned(),
        ));
    }
    match url.scheme() {
        "socks5h" | "http" | "https" => {}
        "socks5" => {
            return Err(ConfigError::Invalid(
                key,
                String::from(
                    "a socks5:// proxy resolves hostnames locally and can never \
                     reach a .onion/.i2p destination; use socks5h:// so the proxy itself \
                     resolves names",
                ),
            ));
        }
        other => {
            return Err(ConfigError::Invalid(
                key,
                format!("unsupported proxy scheme `{other}`; use socks5h:// or http(s)://"),
            ));
        }
    }
    if url.host_str().is_none() {
        return Err(ConfigError::Invalid(
            key,
            "proxy URL has no host".to_owned(),
        ));
    }
    Ok(Some(value))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AuthorizedFetchUnsigned {
    Profile,
    KeyOnly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSmtpConfig {
    server: Option<String>,
    port: Option<u16>,
    login: Option<String>,
    password: Option<String>,
    from_address: Option<String>,
    ssl: Option<bool>,
    starttls: Option<FileStarttls>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileStarttls {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
enum FileTranslationConfig {
    DeepL {
        api_key: Option<String>,
        plan: Option<FileDeepLPlan>,
    },
    Libretranslate {
        endpoint: Option<String>,
        api_key: Option<String>,
    },
    Openai {
        endpoint: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        languages: Option<Vec<String>>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileDeepLPlan {
    Free,
    Pro,
}

impl Config {
    /// Media tools only; the processor entry point never initializes `AppState`,
    /// opens this dummy DSN, acquires an ID lane, or runs application workers.
    pub(crate) fn processor_tools(
        media_dir: PathBuf,
        ffmpeg: String,
        ffprobe: String,
        native: NativeVideoConfig,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::from_parsed(FileConfig {
            domain: Some("processor.invalid".into()),
            database_url: Some("postgres://unused.invalid/processor".into()),
            // This tools-only value is never persisted or used to decrypt
            // origin data. Workers must not need an origin encryption key.
            encryption_secret: Some(crate::auth::generate_secret()),
            media_dir: Some(media_dir),
            ffmpeg_path: Some(ffmpeg),
            ffprobe_path: Some(ffprobe),
            ..FileConfig::default()
        })?;
        config.native_video = native.validate()?;
        Ok(config)
    }
    /// Whether a handle domain identifies this instance. Split-domain peers
    /// commonly try the actor host (`domain`) when an actor was discovered by
    /// URL, so both it and the canonical `account_domain` are local aliases.
    #[must_use]
    pub fn is_local_domain(&self, domain: &str) -> bool {
        domain.eq_ignore_ascii_case(&self.domain)
            || domain.eq_ignore_ascii_case(&self.account_domain)
    }

    /// Base domain below which each Webxdc session gets a separate origin.
    #[must_use]
    pub fn webxdc_domain(&self) -> String {
        format!("webxdc.{}", self.domain)
    }

    /// Cookie-less, storage-isolated browser origin for one Webxdc session.
    #[must_use]
    pub fn webxdc_session_domain(&self, session_id: i64) -> String {
        format!("{session_id}.{}", self.webxdc_domain())
    }

    /// Load and validate a TOML configuration file.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let file: FileConfig = toml::from_str(&contents).map_err(|_| ConfigError::Parse {
            path: path.to_owned(),
        })?;
        Self::from_parsed(file)
    }

    /// Write a documented starter configuration. Existing files are preserved
    /// unless `overwrite` is explicitly true.
    pub fn generate(path: impl AsRef<Path>, overwrite: bool) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(overwrite);
        if !overwrite {
            options.create_new(true);
        }
        let mut file = options.open(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                ConfigError::AlreadyExists(path.to_owned())
            } else {
                ConfigError::Write {
                    path: path.to_owned(),
                    source,
                }
            }
        })?;
        file.write_all(CONFIG_TEMPLATE.as_bytes())
            .map_err(|source| ConfigError::Write {
                path: path.to_owned(),
                source,
            })
    }

    #[allow(clippy::too_many_lines)] // keeps related config validation in one place
    fn from_parsed(file: FileConfig) -> Result<Self, ConfigError> {
        let domain = validate_domain(&required(file.domain, "domain")?)?;
        let account_domain = account_domain_or_host(file.account_domain, &domain)?;
        let database_url = required(file.database_url, "database_url")?;
        let db_pool_size = validate_pool_size(file.database_pool_size)?;
        let trusted_proxies =
            validate_trusted_proxies(file.trusted_proxies.unwrap_or_else(default_trusted_proxies))?;
        let smtp = file
            .smtp
            .map(|smtp| smtp.build(&account_domain))
            .transpose()?;
        let translation = file
            .translation
            .map(FileTranslationConfig::build)
            .transpose()?;
        let altcha = build_altcha_config(file.altcha)?;
        let native_video = file.native_video.unwrap_or_default().build()?;
        let video_offload =
            crate::video_offload::validate_targets(file.video_offload.unwrap_or_default())?;
        let bind = file.bind.unwrap_or_else(|| DEFAULT_BIND.parse().unwrap());
        let live_ingest_bind = file
            .live_ingest_bind
            .unwrap_or_else(|| "127.0.0.1:1935".parse().unwrap());
        let live_rtmps = file
            .live_rtmps
            .map(|rtmps| {
                Ok::<_, ConfigError>(LiveRtmpsConfig {
                    bind: rtmps.bind.ok_or(ConfigError::Missing("live_rtmps.bind"))?,
                    cert: rtmps.cert.ok_or(ConfigError::Missing("live_rtmps.cert"))?,
                    key: rtmps.key.ok_or(ConfigError::Missing("live_rtmps.key"))?,
                })
            })
            .transpose()?;
        if live_rtmps.as_ref().is_some_and(|rtmps| {
            rtmps.bind.port() == 0
                || rtmps.bind.port() == bind.port()
                || rtmps.bind.port() == live_ingest_bind.port()
        }) {
            return Err(ConfigError::Invalid(
                "live_rtmps.bind",
                "must use a nonzero port different from the HTTP and plaintext RTMP listeners"
                    .to_owned(),
            ));
        }

        let encryption_secret_value = required(file.encryption_secret, "encryption_secret")?;
        if encryption_secret_value.len() < 32 {
            return Err(ConfigError::Invalid(
                "encryption_secret",
                "must contain at least 32 bytes of entropy-bearing material".to_owned(),
            ));
        }
        let encryption_secret = Some(SecretString::from(encryption_secret_value));
        let encryption_secret_version = file.encryption_secret_version.unwrap_or(1);
        if encryption_secret_version <= 0 {
            return Err(ConfigError::Invalid(
                "encryption_secret_version",
                "must be a positive integer".to_owned(),
            ));
        }
        let mut encryption_previous_secrets = Vec::new();
        let mut encryption_versions = std::collections::HashSet::new();
        encryption_versions.insert(encryption_secret_version);
        for entry in file.encryption_previous_secrets.unwrap_or_default() {
            let (raw_version, secret) = entry.split_once(':').ok_or_else(|| {
                ConfigError::Invalid(
                    "encryption_previous_secrets",
                    "entries must use VERSION:SECRET".to_owned(),
                )
            })?;
            let version = raw_version.parse::<i32>().map_err(|_| {
                ConfigError::Invalid(
                    "encryption_previous_secrets",
                    "key version must be an integer".to_owned(),
                )
            })?;
            if version <= 0 || secret.len() < 32 || !encryption_versions.insert(version) {
                return Err(ConfigError::Invalid(
                    "encryption_previous_secrets",
                    "versions must be positive and unique and secrets at least 32 bytes".to_owned(),
                ));
            }
            encryption_previous_secrets.push((version, SecretString::from(secret)));
        }
        let allow_private_fetch = file.allow_private_fetch.unwrap_or(false);
        let federation = file
            .federation
            .map(FileFederationConfig::build)
            .transpose()?
            .unwrap_or_default();
        if federation.proxy_url.is_some()
            && !allow_private_fetch
            && !federation.trust_proxy_destination_filtering
        {
            return Err(ConfigError::Invalid(
                "federation.trust_proxy_destination_filtering",
                "a global proxy resolves clearnet destinations outside Plamenu's SSRF guard; set \
                 this acknowledgement only after enforcing the documented proxy egress ACL, or \
                 use onion_proxy_url/i2p_proxy_url instead"
                    .to_owned(),
            ));
        }
        Ok(Self {
            domain,
            account_domain,
            database_url,
            db_pool_size,
            bind,
            allow_private_fetch,
            authorized_fetch: file.authorized_fetch.unwrap_or(true),
            authorized_fetch_unsigned_profile: matches!(
                file.authorized_fetch_unsigned
                    .unwrap_or(AuthorizedFetchUnsigned::Profile),
                AuthorizedFetchUnsigned::Profile
            ),
            media_dir: file.media_dir.unwrap_or_else(|| DEFAULT_MEDIA_DIR.into()),
            media_mirror: match file.media_mirror {
                Some(mirror)
                    if mirror.directory.as_os_str().is_empty()
                        || mirror.bandwidth_mib_per_second == 0
                        || mirror.bandwidth_mib_per_second > 1024
                        || mirror.min_free_gib > 1024 * 1024 =>
                {
                    return Err(ConfigError::Invalid(
                        "media_mirror",
                        "requires a directory, 1–1024 MiB/s and a bounded free-space floor".into(),
                    ));
                }
                mirror => mirror,
            },
            ffmpeg_path: file.ffmpeg_path.unwrap_or_else(|| "ffmpeg".to_owned()),
            video_redundancy: file
                .video_redundancy
                .map(VideoRedundancyConfig::validate)
                .transpose()?,
            video_analytics: file.video_analytics.unwrap_or(true),
            ffprobe_path: file.ffprobe_path.unwrap_or_else(|| "ffprobe".to_owned()),
            live_ingest_bind,
            live_rtmps,
            native_video,
            video_offload,
            smtp,
            trusted_proxies,
            encryption_secret,
            encryption_secret_version,
            encryption_previous_secrets,
            altcha,
            update_check_url: nonempty(file.update_check_url),
            translation,
            conversation_containers: file.conversation_containers.unwrap_or(false),
            csp_reporting: file.csp_reporting.unwrap_or(false),
            federation,
        })
    }

    /// A startup warning when the server is reachable off-host yet trusts a
    /// broad private range as a proxy: any host on that range could then supply
    /// `X-Forwarded-For` and control the client IP used for rate limits, IP
    /// blocks, and audit records. Returns `None` for a loopback bind or a
    /// tightly scoped proxy list. Pure so it can be unit-tested directly.
    #[must_use]
    pub fn insecure_proxy_trust_warning(&self) -> Option<String> {
        if self.bind.ip().is_loopback() {
            return None;
        }
        let broad: Vec<&str> = self
            .trusted_proxies
            .iter()
            .filter(|cidr| proxy_cidr_is_broad(cidr))
            .map(String::as_str)
            .collect();
        if broad.is_empty() {
            return None;
        }
        Some(format!(
            "trusted_proxies trusts broad private range(s) [{}] while binding to a \
             non-loopback address ({}); any host on those ranges can spoof \
             X-Forwarded-For and forge the client IP used for rate limits, IP blocks, \
             and audit logs. Restrict trusted_proxies to your reverse proxy's exact \
             address or subnet.",
            broad.join(", "),
            self.bind
        ))
    }

    /// Global proxy-side DNS bypasses Plamenu's connector resolver. Even with
    /// an explicit acknowledgement, keep that changed trust boundary visible
    /// at every startup.
    #[must_use]
    pub fn proxy_destination_filtering_warning(&self) -> Option<&'static str> {
        (self.federation.proxy_url.is_some() && !self.allow_private_fetch).then_some(
            "global federation proxy is enabled: the proxy egress ACL, not Plamenu's DNS \
             resolver, is authoritative for clearnet destination filtering",
        )
    }
}

impl FileSmtpConfig {
    fn build(self, domain: &str) -> Result<SmtpConfig, ConfigError> {
        Ok(SmtpConfig {
            server: required(self.server, "smtp.server")?,
            port: self.port.unwrap_or(587),
            login: nonempty(self.login),
            password: nonempty(self.password),
            from_address: nonempty(self.from_address)
                .unwrap_or_else(|| format!("notifications@{domain}")),
            ssl: self.ssl.unwrap_or(false),
            starttls: match self.starttls.unwrap_or(FileStarttls::Auto) {
                FileStarttls::Auto => Starttls::Auto,
                FileStarttls::Always => Starttls::Always,
                FileStarttls::Never => Starttls::Never,
            },
        })
    }
}

impl FileTranslationConfig {
    fn build(self) -> Result<TranslationConfig, ConfigError> {
        match self {
            Self::DeepL { api_key, plan } => Ok(TranslationConfig::DeepL {
                api_key: required(api_key, "translation.api_key")?,
                plan: match plan.unwrap_or(FileDeepLPlan::Free) {
                    FileDeepLPlan::Free => DeepLPlan::Free,
                    FileDeepLPlan::Pro => DeepLPlan::Pro,
                },
            }),
            Self::Libretranslate { endpoint, api_key } => Ok(TranslationConfig::LibreTranslate {
                endpoint: required(endpoint, "translation.endpoint")?
                    .trim_end_matches('/')
                    .to_owned(),
                api_key: nonempty(api_key),
            }),
            Self::Openai {
                endpoint,
                model,
                api_key,
                languages,
            } => Ok(TranslationConfig::OpenAi {
                endpoint: required(endpoint, "translation.endpoint")?
                    .trim_end_matches('/')
                    .to_owned(),
                model: required(model, "translation.model")?,
                api_key: nonempty(api_key),
                languages: match languages {
                    Some(list) if !list.is_empty() => list,
                    _ => HY_MT2_LANGUAGES.iter().map(|&l| l.to_owned()).collect(),
                },
            }),
        }
    }
}

/// The 33 languages Hy-MT2 (the model the `openai` backend is built around)
/// translates between, from its model card. Overridable per deployment via
/// `translation.languages`.
const HY_MT2_LANGUAGES: &[&str] = &[
    "zh", "zh-Hant", "yue", "en", "fr", "pt", "es", "ja", "tr", "ru", "ar", "ko", "th", "it", "de",
    "vi", "ms", "id", "tl", "hi", "pl", "cs", "nl", "km", "my", "fa", "gu", "ur", "te", "mr", "he",
    "bn", "ta", "uk", "bo", "kk", "mn", "ug",
];

fn required(value: Option<String>, key: &'static str) -> Result<String, ConfigError> {
    nonempty(value).ok_or(ConfigError::Missing(key))
}

/// Validates and normalizes the operator-supplied public hostname. It becomes
/// part of permanent `ActivityPub` object IDs and the `WebAuthn` relying-party
/// id, so it must be a bare DNS hostname: no scheme, port, path, userinfo,
/// whitespace, or IP literal. Returns the lower-cased hostname. Malformed values
/// are rejected here as a `ConfigError` at load time instead of panicking later
/// in `WebAuthn` construction (a bare non-empty check used to let them through).
fn validate_domain(value: &str) -> Result<String, ConfigError> {
    let domain = value.trim();
    let invalid = |reason: &str| ConfigError::Invalid("domain", format!("{reason} ({domain:?})"));
    if domain.is_empty() {
        return Err(ConfigError::Missing("domain"));
    }
    if domain.chars().any(char::is_whitespace) {
        return Err(invalid("must not contain whitespace"));
    }
    // A bare hostname has none of these; each marks a scheme, port, path,
    // userinfo, query, or fragment that does not belong in `domain`.
    if let Some(bad) = domain
        .chars()
        .find(|c| matches!(c, ':' | '/' | '@' | '?' | '#' | '\\'))
    {
        return Err(invalid(&format!("must be a bare hostname without `{bad}`")));
    }
    // An IP literal is not a valid WebAuthn relying-party id.
    if domain.parse::<IpAddr>().is_ok() {
        return Err(invalid("must be a hostname, not an IP address"));
    }
    let domain = domain.to_ascii_lowercase();
    if domain.len() > 253 {
        return Err(invalid("hostname is longer than 253 characters"));
    }
    // DNS label syntax: dot-separated labels of [a-z0-9-], 1..=63 characters,
    // no leading or trailing hyphen.
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid("each hostname label must be 1..=63 characters"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("hostname labels must not start or end with `-`"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(invalid(
                "hostname labels may contain only letters, digits, and `-`",
            ));
        }
    }
    // The definitive check: prove the domain actually builds a WebAuthn relying
    // party, the exact construction `AppState::new` performs. This keeps that
    // path's invariant honest — any relying-party rejection is surfaced here as
    // a config error rather than a startup panic.
    crate::state::try_build_webauthn(&domain)
        .map_err(|reason| ConfigError::Invalid("domain", reason))?;
    Ok(domain)
}

/// Validates the optional canonical handle domain. Reusing the hosting-domain
/// hostname checks keeps both sides of a split deployment ASCII/DNS-safe; the
/// field-specific error remap makes startup diagnostics name the operator's
/// actual setting. Matching `GoToSocial`'s guard, the HTTPS host must be the
/// account domain itself or one of its subdomains (for example
/// `social.example.com` under `example.com`).
fn validate_account_domain(value: &str, host_domain: &str) -> Result<String, ConfigError> {
    let account_domain = validate_domain(value).map_err(|error| match error {
        ConfigError::Missing(_) => ConfigError::Missing("account_domain"),
        ConfigError::Invalid(_, message) => ConfigError::Invalid("account_domain", message),
        other => other,
    })?;
    if host_domain != account_domain
        && !host_domain
            .strip_suffix(&account_domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
    {
        return Err(ConfigError::Invalid(
            "account_domain",
            format!(
                "hosting domain {host_domain:?} must equal the account domain or be its subdomain"
            ),
        ));
    }
    Ok(account_domain)
}

fn account_domain_or_host(
    configured: Option<String>,
    host_domain: &str,
) -> Result<String, ConfigError> {
    nonempty(configured).map_or_else(
        || Ok(host_domain.to_owned()),
        |value| validate_account_domain(&value, host_domain),
    )
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Validates the operator-supplied connection-pool size, defaulting to
/// [`DEFAULT_DB_POOL_SIZE`] when unset. Rejects `0` (the pool must be able to
/// serve at least one request) and values above [`MAX_DB_POOL_SIZE`] (a typo
/// that would try to exhaust `PostgreSQL`'s own connection limit).
fn validate_pool_size(value: Option<u32>) -> Result<u32, ConfigError> {
    let size = value.unwrap_or(DEFAULT_DB_POOL_SIZE);
    if !(1..=MAX_DB_POOL_SIZE).contains(&size) {
        return Err(ConfigError::Invalid(
            "database_pool_size",
            format!("must be between 1 and {MAX_DB_POOL_SIZE}, got {size}"),
        ));
    }
    Ok(size)
}

fn validate_trusted_proxies(entries: Vec<String>) -> Result<Vec<String>, ConfigError> {
    entries
        .into_iter()
        .map(|entry| {
            let entry = entry.trim();
            let cidr = if entry.contains('/') {
                entry.to_owned()
            } else {
                let addr: IpAddr = entry.parse().map_err(|error: std::net::AddrParseError| {
                    ConfigError::Invalid("trusted_proxies", error.to_string())
                })?;
                format!("{addr}/{}", if addr.is_ipv4() { 32 } else { 128 })
            };
            let (address, prefix) = cidr
                .rsplit_once('/')
                .ok_or_else(|| ConfigError::Invalid("trusted_proxies", cidr.clone()))?;
            let address: IpAddr = address
                .parse()
                .map_err(|_| ConfigError::Invalid("trusted_proxies", cidr.clone()))?;
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| ConfigError::Invalid("trusted_proxies", cidr.clone()))?;
            if prefix > if address.is_ipv4() { 32 } else { 128 } {
                return Err(ConfigError::Invalid("trusted_proxies", cidr));
            }
            Ok(cidr)
        })
        .collect()
}

fn default_trusted_proxies() -> Vec<String> {
    // Loopback only. A reverse proxy on any other address must be listed
    // explicitly (see `deploy/plamenu.toml.example`): trusting the broad
    // private ranges by default let any host on those networks spoof
    // `X-Forwarded-For` and forge the client IP used for rate limits, IP
    // blocks, and login audit records. `insecure_proxy_trust_warning` flags the
    // remaining broad-range configurations at startup.
    ["127.0.0.0/8", "::1/128"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// True when a validated `trusted_proxies` CIDR spans more than a single host
/// and is not a loopback range — i.e. it can contain hosts other than the
/// intended reverse proxy. Loopback (`127.0.0.0/8`, `::1/128`) is never "broad"
/// because it only ever reaches this machine.
fn proxy_cidr_is_broad(cidr: &str) -> bool {
    let Some((address, prefix)) = cidr.rsplit_once('/') else {
        return false;
    };
    let (Ok(address), Ok(prefix)) = (address.parse::<IpAddr>(), prefix.parse::<u8>()) else {
        return false;
    };
    if address.is_loopback() {
        return false;
    }
    // A shared network rather than a single host or a tightly scoped subnet.
    // The Compose example's `/24` and any explicit `/32`//`/128` stay quiet;
    // the big RFC1918 / ULA blocks (`/8`, `/12`, `/16`, `/7`) trip the warning.
    match address {
        IpAddr::V4(_) => prefix < 24,
        IpAddr::V6(_) => prefix < 64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn federation_mirror_limits_are_optional_bounded_and_deadline_compatible() {
        assert!(VideoRedundancyConfig::default().validate().is_ok());
        for value in [
            VideoRedundancyConfig {
                quota_gib: 0,
                ..Default::default()
            },
            VideoRedundancyConfig {
                max_mirror_mib: 4097,
                ..Default::default()
            },
            VideoRedundancyConfig {
                bandwidth_mib_per_second: 0,
                ..Default::default()
            },
            VideoRedundancyConfig {
                retention_hours: 169,
                ..Default::default()
            },
            VideoRedundancyConfig {
                quota_gib: 1,
                max_mirror_mib: 1025,
                ..Default::default()
            },
            VideoRedundancyConfig {
                quota_gib: 4,
                max_mirror_mib: 4096,
                bandwidth_mib_per_second: 1,
                ..Default::default()
            },
        ] {
            assert!(value.validate().is_err());
        }
    }

    fn valid_minimal() -> &'static str {
        "domain = \"example.com\"\ndatabase_url = \"postgres://localhost/plamenu\"\n\
         encryption_secret = \"unit-test-encryption-secret-at-least-32-bytes\"\n"
    }

    #[test]
    fn generated_config_is_valid_but_requires_operator_values() {
        let file: FileConfig = toml::from_str(CONFIG_TEMPLATE).unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Missing("domain"))
        ));
    }

    #[test]
    fn generated_optional_tables_do_not_capture_required_root_settings() {
        for header in [
            "[media_mirror]",
            "[video_redundancy]",
            "[live_rtmps]",
            "[native_video]",
            "[[video_offload]]",
            "[altcha]",
        ] {
            let mut active = false;
            let enabled = CONFIG_TEMPLATE
                .lines()
                .map(|line| {
                    if line == format!("# {header}") {
                        active = true;
                        return header.to_owned();
                    }
                    if line.is_empty() || line.starts_with("# [") {
                        active = false;
                    }
                    if active && line.starts_with("# ") && line.contains(" = ") {
                        line[2..].to_owned()
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let file: FileConfig =
                toml::from_str(&enabled).unwrap_or_else(|error| panic!("{header}: {error}"));
            assert_eq!(
                file.encryption_secret.as_deref(),
                Some(""),
                "{header} must preserve root encryption settings"
            );
            assert_eq!(file.encryption_secret_version, Some(1), "{header}");
        }
    }

    #[test]
    fn compose_deployment_example_is_a_valid_config() {
        let example = include_str!("../../../deploy/plamenu.toml.example");
        let file: FileConfig = toml::from_str(example).unwrap();
        let config = Config::from_parsed(file).unwrap();
        assert_eq!(config.domain, "social.example.com");
        assert_eq!(config.bind, "0.0.0.0:8420".parse().unwrap());
        assert_eq!(config.trusted_proxies, ["172.30.0.0/24"]);
    }

    #[test]
    fn host_deployment_example_is_a_valid_config() {
        let example = include_str!("../../../deploy/plamenu.host.toml.example");
        let file: FileConfig = toml::from_str(example).unwrap();
        let config = Config::from_parsed(file).unwrap();
        assert_eq!(config.domain, "social.example.com");
        assert_eq!(config.bind, "127.0.0.1:8420".parse().unwrap());
        assert_eq!(
            config.database_url,
            "postgres://plamenu@%2Fvar%2Frun%2Fpostgresql/plamenu"
        );
        assert_eq!(config.trusted_proxies, ["127.0.0.1/32", "::1/128"]);
    }

    #[test]
    fn native_rtmps_is_optional_but_requires_complete_distinct_listener() {
        let plain: FileConfig = toml::from_str(valid_minimal()).unwrap();
        assert!(Config::from_parsed(plain).unwrap().live_rtmps.is_none());

        for fragment in [
            "[live_rtmps]\nbind = \"127.0.0.1:1936\"\n",
            "[live_rtmps]\ncert = \"cert.pem\"\nkey = \"key.pem\"\n",
            "[live_rtmps]\nbind = \"127.0.0.1:1935\"\ncert = \"cert.pem\"\nkey = \"key.pem\"\n",
        ] {
            let file: FileConfig =
                toml::from_str(&format!("{}{fragment}", valid_minimal())).unwrap();
            assert!(Config::from_parsed(file).is_err());
        }
        let file: FileConfig = toml::from_str(&format!(
            "{}[live_rtmps]\nbind = \"0.0.0.0:1936\"\ncert = \"cert.pem\"\nkey = \"key.pem\"\n",
            valid_minimal()
        ))
        .unwrap();
        assert_eq!(
            Config::from_parsed(file)
                .unwrap()
                .live_rtmps
                .unwrap()
                .bind
                .port(),
            1936
        );
    }

    #[test]
    fn encryption_root_is_a_startup_required_setting() {
        let file: FileConfig = toml::from_str(
            "domain = \"example.com\"\ndatabase_url = \"postgres://localhost/plamenu\"\n",
        )
        .unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Missing("encryption_secret"))
        ));
    }

    #[test]
    fn minimal_config_uses_built_in_defaults() {
        let file: FileConfig = toml::from_str(valid_minimal()).unwrap();
        let config = Config::from_parsed(file).unwrap();
        assert_eq!(config.bind, DEFAULT_BIND.parse().unwrap());
        assert!(config.authorized_fetch);
        assert!(config.authorized_fetch_unsigned_profile);
        assert!(!config.conversation_containers);
        assert!(!config.csp_reporting);
        assert!(config.smtp.is_none());
        assert!(config.translation.is_none());
        assert_eq!(config.db_pool_size, DEFAULT_DB_POOL_SIZE);
        assert_eq!(config.native_video, NativeVideoConfig::default());
        assert!(config.media_mirror.is_none());
        assert!(config.video_offload.is_empty());
        assert!(config.video_analytics);
        assert_eq!(config.account_domain, "example.com");
        assert!(config.is_local_domain("EXAMPLE.COM"));
        assert!(config.federation.proxy_url.is_none());
        assert!(config.federation.onion_proxy_url.is_none());
        assert!(config.federation.i2p_proxy_url.is_none());
        assert!(!config.federation.trust_proxy_destination_filtering);
        assert!(config.federation.no_proxy.is_empty());
    }

    #[test]
    fn optional_mirror_defaults_and_invalid_budgets() {
        let file: FileConfig = toml::from_str(&format!(
            "{}[media_mirror]\ndirectory = \"/disk/replica\"\n",
            valid_minimal()
        ))
        .unwrap();
        let mirror = Config::from_parsed(file).unwrap().media_mirror.unwrap();
        assert_eq!(mirror.bandwidth_mib_per_second, 32);
        assert_eq!(mirror.min_free_gib, 1);
        for value in [
            "directory = \"\"",
            "directory = \"/disk\"\nbandwidth_mib_per_second = 0",
            "directory = \"/disk\"\nbandwidth_mib_per_second = 1025",
            "directory = \"/disk\"\nmin_free_gib = 18446744073709551615",
        ] {
            let file: FileConfig =
                toml::from_str(&format!("{}[media_mirror]\n{value}\n", valid_minimal())).unwrap();
            assert!(matches!(
                Config::from_parsed(file),
                Err(ConfigError::Invalid("media_mirror", _))
            ));
        }
    }

    #[test]
    fn csp_reporting_requires_an_explicit_opt_in() {
        let toml = format!("{}csp_reporting = true\n", valid_minimal());
        let file: FileConfig = toml::from_str(&toml).unwrap();
        assert!(Config::from_parsed(file).unwrap().csp_reporting);
    }

    #[test]
    fn native_video_limits_are_adjustable_and_reject_invalid_budgets() {
        let input = format!(
            "{}[native_video]\nmax_upload_mib = 4096\nper_account_gib = 200\nglobal_gib = 500\npending_gib = 12\nunfinished_drafts = 24\njobs = 3\nmin_free_gib = 2\n",
            valid_minimal()
        );
        let file: FileConfig = toml::from_str(&input).unwrap();
        let video = Config::from_parsed(file).unwrap().native_video;
        assert_eq!(video.max_upload_bytes(), 4 * 1024 * NativeVideoConfig::MIB);
        assert_eq!(video.jobs, 3);
        assert_eq!(video.unfinished_drafts, 24);
        assert_eq!(video.output_limit(1), 256 * NativeVideoConfig::MIB);
        for value in [
            "max_upload_mib = 0",
            "jobs = 0",
            "unfinished_drafts = 129",
            "global_gib = 0",
            "per_account_gib = 70000",
        ] {
            let file: FileConfig =
                toml::from_str(&format!("{}[native_video]\n{value}\n", valid_minimal())).unwrap();
            assert!(matches!(
                Config::from_parsed(file),
                Err(ConfigError::Invalid("native_video", _))
            ));
        }
    }

    #[test]
    fn federation_section_parses_and_normalizes() {
        let toml = format!(
            "{}[federation]\n\
             onion_proxy_url = \"socks5h://127.0.0.1:9050\"\n\
             no_proxy = [\" Internal.Example \", \"\"]\n",
            valid_minimal()
        );
        let file: FileConfig = toml::from_str(&toml).unwrap();
        let config = Config::from_parsed(file).unwrap();
        assert_eq!(
            config.federation.onion_proxy_url.as_deref(),
            Some("socks5h://127.0.0.1:9050")
        );
        assert!(config.federation.proxy_url.is_none());
        // Entries are trimmed, lowercased, and empties dropped.
        assert_eq!(config.federation.no_proxy, ["internal.example"]);
    }

    /// `socks5://` (no `h`) resolves hostnames locally — verified against
    /// reqwest 0.13, where it fails for every `.onion` — so it is rejected at
    /// load with an error that names the fix.
    #[test]
    fn federation_proxy_rejects_local_resolving_socks5() {
        let with = |line: &str| {
            let toml = format!("{}[federation]\n{line}\n", valid_minimal());
            let file: FileConfig = toml::from_str(&toml).unwrap();
            Config::from_parsed(file)
        };
        match with("onion_proxy_url = \"socks5://127.0.0.1:9050\"") {
            Err(ConfigError::Invalid("federation.onion_proxy_url", message)) => {
                assert!(
                    message.contains("socks5h"),
                    "error must name the fix: {message}"
                );
            }
            other => panic!("expected socks5 rejection, got {other:?}"),
        }
        assert!(matches!(
            with("proxy_url = \"ftp://127.0.0.1:21\""),
            Err(ConfigError::Invalid("federation.proxy_url", _))
        ));
        assert!(matches!(
            with("i2p_proxy_url = \"not a url\""),
            Err(ConfigError::Invalid("federation.i2p_proxy_url", _))
        ));
        // Per-overlay supported schemes load without changing clearnet trust.
        assert!(with("onion_proxy_url = \"socks5h://127.0.0.1:9050\"").is_ok());
        // A global proxy is rejected until the operator explicitly accepts
        // proxy-side destination filtering.
        assert!(matches!(
            with("proxy_url = \"http://127.0.0.1:8118\""),
            Err(ConfigError::Invalid(
                "federation.trust_proxy_destination_filtering",
                _
            ))
        ));
        assert!(
            with(
                "proxy_url = \"http://127.0.0.1:8118\"\n\
                 trust_proxy_destination_filtering = true"
            )
            .is_ok()
        );
        // Credential-bearing proxy URLs are never reflected in errors.
        let sentinel = "proxy-secret-sentinel";
        let error = with(&format!(
            "onion_proxy_url = \"socks5h://user:{sentinel}@127.0.0.1:9050\""
        ))
        .unwrap_err()
        .to_string();
        assert!(!error.contains(sentinel), "secret leaked in {error}");
    }

    #[test]
    fn configuration_debug_and_parse_errors_redact_secrets() {
        use std::io::Write as _;

        let sentinel = "unique-secret-sentinel";
        let toml = format!(
            "domain = \"example.com\"\n\
             database_url = \"postgres://user:{sentinel}@db/plamenu\"\n\
             encryption_secret = \"{sentinel}-encryption-material-long-enough\"\n\
             [smtp]\nserver = \"smtp.example.com\"\nlogin = \"{sentinel}\"\n\
             password = \"{sentinel}\"\n\
             [translation]\nbackend = \"deepl\"\napi_key = \"{sentinel}\"\n"
        );
        let file: FileConfig = toml::from_str(&toml).unwrap();
        let debug = format!("{:?}", Config::from_parsed(file).unwrap());
        assert!(!debug.contains(sentinel), "secret leaked in {debug}");

        let mut invalid = tempfile::NamedTempFile::new().unwrap();
        write!(
            invalid,
            "database_url = \"postgres://user:{sentinel}@db/plamenu\"\nbroken = ["
        )
        .unwrap();
        let error = Config::from_file(invalid.path()).unwrap_err().to_string();
        assert!(!error.contains(sentinel), "secret leaked in {error}");
    }

    #[test]
    fn database_pool_size_is_operator_configurable_within_bounds() {
        let with = |line: &str| {
            let file: FileConfig = toml::from_str(&format!("{}{line}\n", valid_minimal())).unwrap();
            Config::from_parsed(file)
        };
        // A valid custom size is honoured.
        assert_eq!(with("database_pool_size = 40").unwrap().db_pool_size, 40);
        assert_eq!(with("database_pool_size = 1").unwrap().db_pool_size, 1);
        assert_eq!(
            with(&format!("database_pool_size = {MAX_DB_POOL_SIZE}"))
                .unwrap()
                .db_pool_size,
            MAX_DB_POOL_SIZE
        );
        // Zero and over-ceiling values are rejected at load, not silently used.
        for bad in ["database_pool_size = 0", "database_pool_size = 5000"] {
            assert!(
                matches!(
                    with(bad),
                    Err(ConfigError::Invalid("database_pool_size", _))
                ),
                "expected {bad:?} to be rejected",
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error =
            toml::from_str::<FileConfig>(&format!("{}wat = true\n", valid_minimal())).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn enabled_sections_validate_their_required_values() {
        let file: FileConfig =
            toml::from_str(&format!("{}[smtp]\nserver = \"\"\n", valid_minimal())).unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Missing("smtp.server"))
        ));

        let file: FileConfig = toml::from_str(&format!(
            "{}[translation]\nbackend = \"deepl\"\napi_key = \"\"\n",
            valid_minimal()
        ))
        .unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Missing("translation.api_key"))
        ));

        let file: FileConfig = toml::from_str(&format!(
            "{}[translation]\nbackend = \"openai\"\nmodel = \"hy-mt2-1.8b\"\n",
            valid_minimal()
        ))
        .unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Missing("translation.endpoint"))
        ));
    }

    #[test]
    fn openai_backend_defaults_to_the_hy_mt2_languages() {
        let file: FileConfig = toml::from_str(&format!(
            "{}[translation]\nbackend = \"openai\"\nendpoint = \"http://mt.local:8080/\"\nmodel = \"hy-mt2-1.8b\"\n",
            valid_minimal()
        ))
        .unwrap();
        let config = Config::from_parsed(file).unwrap();
        let Some(TranslationConfig::OpenAi {
            endpoint,
            model,
            api_key,
            languages,
        }) = config.translation
        else {
            panic!("openai backend expected");
        };
        assert_eq!(endpoint, "http://mt.local:8080");
        assert_eq!(model, "hy-mt2-1.8b");
        assert_eq!(api_key, None);
        assert!(languages.iter().any(|l| l == "uk"));
        assert_eq!(languages.len(), HY_MT2_LANGUAGES.len());
    }

    #[test]
    fn valid_domains_are_accepted_and_normalized() {
        for (input, expected) in [
            ("example.com", "example.com"),
            ("  example.com  ", "example.com"),
            ("Sub.Example.COM", "sub.example.com"),
            ("plamenu.local", "plamenu.local"),
            ("a-b.example.io", "a-b.example.io"),
        ] {
            assert_eq!(validate_domain(input).unwrap(), expected, "input {input:?}");
        }
    }

    #[test]
    fn split_account_domain_is_normalized_and_requires_the_host_below_it() {
        let split = format!(
            "{}account_domain = \"Example.COM\"\n",
            valid_minimal().replace("example.com", "Social.Example.COM")
        );
        let file: FileConfig = toml::from_str(&split).unwrap();
        let config = Config::from_parsed(file).unwrap();
        assert_eq!(config.domain, "social.example.com");
        assert_eq!(config.account_domain, "example.com");
        assert!(config.is_local_domain("example.com"));
        assert!(config.is_local_domain("social.example.com"));
        assert!(!config.is_local_domain("elsewhere.example"));

        for account_domain in ["elsewhere.test", "deep.social.example.com"] {
            let input = format!(
                "{}account_domain = \"{account_domain}\"\n",
                valid_minimal().replace("example.com", "social.example.com")
            );
            let file: FileConfig = toml::from_str(&input).unwrap();
            assert!(matches!(
                Config::from_parsed(file),
                Err(ConfigError::Invalid("account_domain", _))
            ));
        }
    }

    #[test]
    fn malformed_domains_are_rejected_at_load_instead_of_panicking() {
        for bad in [
            "https://example.com", // scheme
            "example.com/inbox",   // path
            "example.com:8443",    // port
            "user@example.com",    // userinfo
            "exa mple.com",        // whitespace
            "example..com",        // empty label
            "-example.com",        // leading hyphen
            "example-.com",        // trailing hyphen
            "192.168.1.1",         // IPv4 literal
            "example.com?a=b",     // query
            "exàmple.com",         // non-ASCII label
        ] {
            assert!(
                matches!(validate_domain(bad), Err(ConfigError::Invalid("domain", _))),
                "expected {bad:?} to be rejected as invalid",
            );
        }
    }

    #[test]
    fn invalid_domain_config_fails_to_load() {
        let file: FileConfig =
            toml::from_str("domain = \"https://example.com\"\ndatabase_url = \"postgres://x/y\"\n")
                .unwrap();
        assert!(matches!(
            Config::from_parsed(file),
            Err(ConfigError::Invalid("domain", _))
        ));
    }

    #[test]
    fn default_trusted_proxies_are_loopback_only() {
        // Narrowed from the old broad private-range default: a proxy on any
        // other address must now be listed explicitly.
        assert_eq!(default_trusted_proxies(), ["127.0.0.0/8", "::1/128"]);
    }

    #[test]
    fn proxy_cidr_is_broad_flags_only_shared_ranges() {
        for broad in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"] {
            assert!(proxy_cidr_is_broad(broad), "{broad} should be broad");
        }
        for narrow in [
            "127.0.0.0/8", // loopback, however wide, only reaches this host
            "::1/128",
            "172.30.0.0/24", // the Compose example's scoped subnet
            "203.0.113.7/32",
            "2001:db8::1/128",
            "fd00::/64",
        ] {
            assert!(!proxy_cidr_is_broad(narrow), "{narrow} should be narrow");
        }
    }

    fn config_with(bind: &str, trusted: &[&str]) -> Config {
        let file: FileConfig = toml::from_str(valid_minimal()).unwrap();
        let mut config = Config::from_parsed(file).unwrap();
        config.bind = bind.parse().unwrap();
        config.trusted_proxies = trusted.iter().map(|s| (*s).to_owned()).collect();
        config
    }

    #[test]
    fn insecure_proxy_trust_warning_fires_only_for_broad_trust_off_host() {
        // Off-host bind + a broad range → warn, and name the offending range.
        let warning = config_with("0.0.0.0:8420", &["10.0.0.0/8"])
            .insecure_proxy_trust_warning()
            .expect("a broad range on an off-host bind must warn");
        assert!(warning.contains("10.0.0.0/8"), "{warning}");

        // The default (loopback-only) is quiet even on the default 0.0.0.0 bind.
        assert!(
            config_with("0.0.0.0:8420", &["127.0.0.0/8", "::1/128"])
                .insecure_proxy_trust_warning()
                .is_none()
        );
        // The Compose example's scoped /24 is quiet.
        assert!(
            config_with("0.0.0.0:8420", &["172.30.0.0/24"])
                .insecure_proxy_trust_warning()
                .is_none()
        );
        // A loopback bind is quiet even with a broad range configured.
        assert!(
            config_with("127.0.0.1:8420", &["10.0.0.0/8"])
                .insecure_proxy_trust_warning()
                .is_none()
        );
    }
}
