use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use plamenu_db::instance_settings::{self, InstanceSettings};
use plamenu_db::{DbError, PgPool};
use serde_json::Value;
use webauthn_rs::prelude::{Url, Webauthn, WebauthnBuilder};

use crate::Config;
use crate::federation::FederationApi;
use crate::rate_limit::RateLimiter;
use crate::storage::MediaStore;
use crate::streaming::Hub;

/// The admin dashboard metrics cache TTL — Mastodon caches each measure/
/// dimension/cohort for 5 minutes (`BaseMeasure::CACHE_TTL`).
const METRICS_CACHE_TTL: Duration = Duration::from_mins(5);

/// Hard ceiling on distinct cached metrics entries. A dashboard client that
/// varies its window/parameters on every request would otherwise leave every
/// old value resident for the whole process lifetime; the
/// request-side window/keys/limit caps keep each entry's size bounded too.
const METRICS_CACHE_MAX: usize = 512;

/// A tiny in-process TTL cache for the admin metrics endpoints, keyed by the
/// Mastodon-style `cache_key` string (`metrics/<kind>/<key>;start;end;params`).
/// Stands in for Mastodon's server-side cache; the queries are cheap so a plain
/// `Mutex<HashMap>` with lazy eviction is plenty (no new crate dependency,
/// keeping the static-musl build lean).
#[derive(Default)]
pub struct MetricsCache {
    entries: Mutex<HashMap<String, (Instant, Value)>>,
}

impl MetricsCache {
    /// Returns the cached value if present and still within the TTL, evicting it
    /// when stale.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match entries.get(key) {
            Some((stored_at, value)) if stored_at.elapsed() < METRICS_CACHE_TTL => {
                Some(value.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Stores `value` under `key` with a fresh timestamp, evicting so the cache
    /// never exceeds [`METRICS_CACHE_MAX`] live entries.
    pub fn put(&self, key: String, value: Value) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::cache::bounded_ttl_insert(
            &mut entries,
            key,
            value,
            Instant::now(),
            METRICS_CACHE_TTL,
            METRICS_CACHE_MAX,
        );
    }
}

/// How long a cached `instance_settings` row is served before re-reading.
/// Short on purpose: it only exists so per-request consumers (the rate
/// limiter) don't turn every request into a settings query.
const SETTINGS_CACHE_TTL: Duration = Duration::from_secs(5);

/// A TTL cache for the single `instance_settings` row. The admin settings
/// form calls [`SettingsCache::invalidate`] on save so changes apply
/// immediately on this node (other nodes catch up within the TTL). A failed
/// refresh serves the last-known value instead of erroring, so
/// [`SettingsCache::get`] only fails before the first successful load.
///
/// Refreshes are single-flight: when the entry expires, a burst of concurrent
/// requests (the rate limiter runs `get` near the outer request path) would
/// otherwise each miss and independently query `instance_settings`, hammering
/// the shared pool with a stampede every TTL window. A `refresh` async mutex
/// serializes the miss path so exactly one caller queries per window while the
/// rest re-read the value it stores.
pub struct SettingsCache {
    entry: RwLock<Option<(Instant, Arc<InstanceSettings>)>>,
    /// Held across the refresh query so only one caller reloads per TTL window.
    refresh: tokio::sync::Mutex<()>,
    ttl: Duration,
    /// Count of actual `instance_settings` reads performed — the single-flight
    /// invariant is that a burst of misses increments this by one, not by the
    /// number of callers. Also a cheap operational signal of refresh volume.
    fetches: AtomicU64,
}

impl Default for SettingsCache {
    fn default() -> Self {
        Self::new(SETTINGS_CACHE_TTL)
    }
}

impl SettingsCache {
    /// A cache that effectively never expires once loaded. The bench harness
    /// pins settings with this so query counts are deterministic: under the
    /// production 5-second TTL, one `instance_settings` refresh lands inside
    /// whichever measured case happens to straddle the boundary, moving that
    /// case's count by one. Never used by the server itself.
    #[must_use]
    pub fn pinned() -> Self {
        Self::new(Duration::from_secs(u64::MAX / 4))
    }

    fn new(ttl: Duration) -> Self {
        Self {
            entry: RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
            ttl,
            fetches: AtomicU64::new(0),
        }
    }

    /// The cached row if present and still within the TTL. Takes only the short
    /// read lock, never held across an await.
    fn fresh(&self) -> Option<Arc<InstanceSettings>> {
        self.entry
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|(cached_at, settings)| {
                (cached_at.elapsed() < self.ttl).then(|| Arc::clone(settings))
            })
    }

    /// Read settings using a mutation's connection without waiting for a pool
    /// refresh or publishing uncommitted data into the shared cache.
    pub async fn get_on(
        &self,
        conn: &mut plamenu_db::PgConnection,
    ) -> Result<Arc<InstanceSettings>, DbError> {
        if let Some(settings) = self.fresh() {
            return Ok(settings);
        }
        Ok(Arc::new(instance_settings::get(conn).await?))
    }

    pub async fn get(&self, pool: &PgPool) -> Result<Arc<InstanceSettings>, DbError> {
        // Fast path: a fresh value, served without touching the refresh lock.
        if let Some(settings) = self.fresh() {
            return Ok(settings);
        }
        // Miss (empty or expired): serialize the reload so N concurrent misses
        // become one query instead of N independent hits on the shared pool.
        let _refresh = self.refresh.lock().await;
        // Another caller may have refreshed while we waited for the lock.
        if let Some(settings) = self.fresh() {
            return Ok(settings);
        }
        let fresh = match instance_settings::get(pool).await {
            Ok(row) => Arc::new(row),
            Err(error) => {
                // A failed refresh serves the last-known settings rather than
                // surfacing the outage to every request path: settings are
                // slow-moving operator knobs, and momentary staleness beats an
                // instance-wide error storm during database pressure. The stale
                // entry's timestamp is left alone, so each
                // subsequent miss retries the refresh (serialized by the
                // single-flight lock). Only a cold cache propagates the error.
                let stale = self
                    .entry
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(|(_, settings)| Arc::clone(settings));
                return match stale {
                    Some(settings) => {
                        tracing::warn!(%error, "settings refresh failed; serving last-known settings");
                        Ok(settings)
                    }
                    None => Err(error),
                };
            }
        };
        self.fetches.fetch_add(1, Ordering::Relaxed);
        *self.entry.write().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), Arc::clone(&fresh)));
        Ok(fresh)
    }

    pub fn invalidate(&self) {
        *self.entry.write().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Number of `instance_settings` reads performed since construction — used
    /// by the single-flight tests and available as a refresh-volume signal.
    #[must_use]
    pub fn fetch_count(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }
}

/// How long the translation backend's language map is cached (Mastodon's
/// 7 days) and how long an individual status translation is (its 1 day). The
/// language list is stable and each translation costs a paid API call, so both
/// are worth holding.
const TRANSLATION_LANGUAGES_TTL: Duration = Duration::from_hours(24 * 7);
const TRANSLATION_TTL: Duration = Duration::from_hours(24);

/// A translation backend's `source → [target]` language map.
type LanguageMap = std::collections::BTreeMap<String, Vec<String>>;

/// The in-flight translation locks, keyed by `(status_id, target_language)`.
type InflightMap = HashMap<(i64, String), Arc<tokio::sync::Mutex<()>>>;

/// In-memory (L1) translation entries kept at most; the persistent
/// `status_translations` table is the truth, so overflowing here only costs a
/// database read.
const TRANSLATION_L1_MAX: usize = 1024;

/// Caches the translation backend's `source → [target]` language map and
/// individual status translations, standing in for Mastodon's two
/// server-side translation caches (the language map and per-status
/// translations). Keeping it in-process avoids a network round-trip on
/// every translate request (the permit check needs the language map) and
/// re-paying the backend for an unchanged post.
#[derive(Default)]
pub struct TranslationCache {
    languages: RwLock<Option<(Instant, Arc<LanguageMap>)>>,
    translations: Mutex<HashMap<String, (Instant, Value)>>,
    /// One lock per in-flight `(status, target)` translation: concurrent
    /// requests for the same pair serialize here, so the losers re-read the
    /// caches the winner just filled instead of paying the backend again. Guarded
    /// by a plain `Mutex` (never held across an await) so an [`InflightGuard`]
    /// can reclaim its entry from `Drop` without an async cleanup call.
    inflight: Arc<Mutex<InflightMap>>,
    /// Concurrency gate toward the backend, rebuilt when the operator changes
    /// the `translation_backend_concurrency` setting.
    gate: Mutex<Option<(usize, Arc<tokio::sync::Semaphore>)>>,
}

impl TranslationCache {
    pub fn languages(&self) -> Option<Arc<LanguageMap>> {
        self.languages
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(stored_at, _)| stored_at.elapsed() < TRANSLATION_LANGUAGES_TTL)
            .map(|(_, map)| Arc::clone(map))
    }

    pub fn store_languages(&self, map: Arc<LanguageMap>) {
        *self
            .languages
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), map));
    }

    pub fn translation(&self, key: &str) -> Option<Value> {
        let mut entries = self
            .translations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match entries.get(key) {
            Some((stored_at, value)) if stored_at.elapsed() < TRANSLATION_TTL => {
                Some(value.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn store_translation(&self, key: String, value: Value) {
        let mut entries = self
            .translations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= TRANSLATION_L1_MAX {
            entries.retain(|_, (stored_at, _)| stored_at.elapsed() < TRANSLATION_TTL);
            if entries.len() >= TRANSLATION_L1_MAX {
                // Rare; refills from the persistent cache on demand.
                entries.clear();
            }
        }
        entries.insert(key, (Instant::now(), value));
    }

    /// Claims the per-pair in-flight slot: hold the returned guard's lock across
    /// the miss path so identical concurrent requests wait and then re-read the
    /// caches. The guard removes the map entry from its `Drop`, so the slot is
    /// reclaimed on *every* exit path — success, a `?` error, or cancellation —
    /// instead of leaking on early returns. Stragglers still holding the Arc just
    /// serialize against each other, which is harmless.
    pub fn inflight_lock(&self, status_id: i64, target: &str) -> InflightGuard {
        let key = (status_id, target.to_owned());
        let lock = Arc::clone(
            self.inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        InflightGuard {
            map: Arc::clone(&self.inflight),
            key,
            lock,
        }
    }

    #[cfg(test)]
    fn inflight_len(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The backend concurrency gate sized to `permits`; replaced (old permits
    /// drain naturally) when the operator changes the setting.
    pub fn backend_gate(&self, permits: usize) -> Arc<tokio::sync::Semaphore> {
        let permits = permits.max(1);
        let mut slot = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        match &*slot {
            Some((size, semaphore)) if *size == permits => Arc::clone(semaphore),
            _ => {
                let semaphore = Arc::new(tokio::sync::Semaphore::new(permits));
                *slot = Some((permits, Arc::clone(&semaphore)));
                semaphore
            }
        }
    }
}

/// The claim on an in-flight `(status, target)` translation slot. It carries the
/// per-pair serialization lock and reclaims the map entry when dropped — however
/// the request finishes — so a fallible or cancelled translation cannot leave a
/// stale `Arc<Mutex>` in the map for the rest of the process.
pub struct InflightGuard {
    map: Arc<Mutex<InflightMap>>,
    key: (i64, String),
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl InflightGuard {
    /// The per-pair lock; hold its guard across the miss path so identical
    /// concurrent requests serialize and then re-read the caches.
    #[must_use]
    pub fn lock(&self) -> &tokio::sync::Mutex<()> {
        &self.lock
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub federation: Arc<dyn FederationApi>,
    pub media: Arc<dyn MediaStore>,
    /// Application-only keyring for normalized federation signing keys. `None`
    /// is tolerated only by test/maintenance states with no encrypted key
    /// rows; production startup preflight fails before workers begin.
    pub federation_keyring: Option<Arc<crate::crypto::FederationKeyring>>,
    /// The streaming subscriber registry; events reach it through the
    /// LISTEN task (`streaming::spawn`).
    pub streaming: Arc<Hub>,
    pub webxdc_realtime: Arc<crate::webxdc_realtime::Hub>,
    /// 5-minute cache for the admin metrics endpoints (`admin_metrics`).
    pub metrics_cache: Arc<MetricsCache>,
    /// Shared counters for the Mastodon-style API throttles (`rate_limit`).
    pub rate_limiter: Arc<RateLimiter>,
    /// Short-TTL cache of the `instance_settings` row for per-request reads.
    pub settings_cache: Arc<SettingsCache>,
    /// The `WebAuthn` relying party. Its `rp_id`/origin are pinned to
    /// [`Config::domain`], so security-key ceremonies are bound to this host.
    pub webauthn: Arc<Webauthn>,
    /// Caches the translation backend's language map and per-status
    /// translations (M25).
    pub translation_cache: Arc<TranslationCache>,
    /// Coalesces + rate-limits the best-effort remote-collection refreshes
    /// (`featuredTags`/`outbox`) spawned on each accepted actor `Update`, so a
    /// flood of signed self-`Update`s can't grow the detached-task population
    /// without bound.
    pub remote_refresh: Arc<crate::remote::RemoteRefreshCoordinator>,
    /// Coalesces the best-effort durable hydration intents spawned by
    /// third-party profile-timeline reads. Keeping this admission synchronous
    /// lets the compatible read path return without adding database round
    /// trips, while bounding detached work under aggressive client refreshes.
    pub remote_history_intent: Arc<crate::remote_history::IntentCoordinator>,
    /// Lazily-resolved id of the first-party web-session app, cached for the
    /// process lifetime. Lets the auth path age-expire browser sessions
    /// ([`crate::auth::MAX_WEB_SESSION_AGE`]) without touching
    /// third-party API tokens, which keep Mastodon's non-expiring semantics.
    web_app_id_cache: Arc<tokio::sync::OnceCell<i64>>,
    /// Signalled once at shutdown; worker loops observe it through
    /// [`crate::workers::pause`] so they stop claiming work and exit between
    /// jobs instead of being aborted mid-job. Cloned tokens
    /// share one state.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// The supervisors' exit ledger, read by the readiness probe so a
    /// restart-looping subsystem turns the instance unready.
    pub workers: Arc<crate::workers::WorkerRegistry>,
}

/// Builds the `WebAuthn` relying party for `domain` (the relying-party id) with
/// its single allowed origin `https://{domain}` — the only scheme/host the web
/// UI is ever served under. Fallible so config loading can reject a domain that
/// cannot serve as a relying-party id *before* startup instead of panicking:
/// [`crate::config::validate_domain`] runs this exact construction while
/// validating the operator's `domain`.
pub(crate) fn try_build_webauthn(domain: &str) -> Result<Webauthn, String> {
    let origin = Url::parse(&format!("https://{domain}"))
        .map_err(|error| format!("cannot form an https origin: {error}"))?;
    WebauthnBuilder::new(domain, &origin)
        .map_err(|error| format!("not a valid WebAuthn relying-party id: {error}"))?
        .rp_name("Plamenu")
        .build()
        .map_err(|error| format!("WebAuthn relying party failed to build: {error}"))
}

impl AppState {
    /// Assembles the shared state. Fallible because the `WebAuthn` relying
    /// party is rebuilt from `config.domain` here: `validate_domain` proves
    /// the same construction succeeds at config-load time, but the proof is
    /// threaded as a `Result` rather than assumed with a panic — a caller that
    /// skipped config validation gets an error, not a
    /// crash.
    pub fn new(
        pool: PgPool,
        config: Config,
        federation: Arc<dyn FederationApi>,
        media: Arc<dyn MediaStore>,
    ) -> Result<Self, String> {
        let webauthn = Arc::new(
            try_build_webauthn(&config.domain)
                .map_err(|reason| format!("domain {:?}: {reason}", config.domain))?,
        );
        let federation_keyring = if config.encryption_secret.is_some() {
            Some(Arc::new(
                crate::crypto::FederationKeyring::from_config(&config)
                    .map_err(|error| error.to_string())?,
            ))
        } else {
            None
        };
        Ok(Self {
            pool,
            config: Arc::new(config),
            federation,
            media,
            federation_keyring,
            streaming: Arc::new(Hub::default()),
            webxdc_realtime: Arc::new(crate::webxdc_realtime::Hub::default()),
            metrics_cache: Arc::new(MetricsCache::default()),
            rate_limiter: Arc::new(RateLimiter::default()),
            settings_cache: Arc::new(SettingsCache::default()),
            webauthn,
            translation_cache: Arc::new(TranslationCache::default()),
            remote_refresh: Arc::new(crate::remote::RemoteRefreshCoordinator::default()),
            remote_history_intent: Arc::new(crate::remote_history::IntentCoordinator::default()),
            web_app_id_cache: Arc::new(tokio::sync::OnceCell::new()),
            shutdown: tokio_util::sync::CancellationToken::new(),
            workers: Arc::new(crate::workers::WorkerRegistry::default()),
        })
    }

    /// The first-party web-session app's id, resolved once and cached. The web
    /// app is created on first login (`ensure_web_app`); resolving it lazily
    /// yields a stable id on every request past the first, so aging a browser
    /// session out ([`crate::auth::MAX_WEB_SESSION_AGE`]) costs no extra query
    /// on the hot auth path. A transient failure to resolve it is not cached, so
    /// the next request retries.
    pub async fn web_app_id(&self) -> Result<i64, crate::error::ApiError> {
        self.web_app_id_cache
            .get_or_try_init(|| async {
                crate::web::session::ensure_web_app(self)
                    .await
                    .map(|app| app.id)
            })
            .await
            .copied()
    }

    /// The settings row for the effective-value helpers below; `None` when
    /// the read fails (the helpers then fall back to the config file or the
    /// built-in default, so a database hiccup degrades to boot-time behavior
    /// instead of erroring).
    async fn setting_overrides(&self) -> Option<Arc<InstanceSettings>> {
        self.settings_cache.get(&self.pool).await.ok()
    }

    /// Effective authorized-fetch posture (O4): the admin override when set,
    /// otherwise `authorized_fetch` from the config file.
    pub async fn authorized_fetch(&self) -> bool {
        self.setting_overrides()
            .await
            .and_then(|s| s.authorized_fetch)
            .unwrap_or(self.config.authorized_fetch)
    }

    /// Effective unsigned-profile carve-out under secure mode.
    pub async fn authorized_fetch_unsigned_profile(&self) -> bool {
        self.setting_overrides()
            .await
            .and_then(|s| s.authorized_fetch_unsigned_profile)
            .unwrap_or(self.config.authorized_fetch_unsigned_profile)
    }

    /// FEP-8b32 integrity-proof emission on deliveries (settings-owned,
    /// default on).
    pub async fn emit_integrity_proofs(&self) -> bool {
        self.setting_overrides()
            .await
            .is_none_or(|s| s.emit_integrity_proofs)
    }

    /// RFC 9421 double-knock emission on deliveries (settings-owned,
    /// default on).
    pub async fn emit_rfc9421(&self) -> bool {
        self.setting_overrides()
            .await
            .is_none_or(|s| s.emit_rfc9421)
    }

    /// Effective FEP-171b conversation-container ownership.
    pub async fn conversation_containers(&self) -> bool {
        self.setting_overrides()
            .await
            .and_then(|s| s.conversation_containers)
            .unwrap_or(self.config.conversation_containers)
    }

    /// Sign-in-log/IP retention in days (settings-owned, default a year).
    pub async fn ip_retention_days(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(365, |s| s.ip_retention_days)
    }

    /// Days a cached copy of remote media is kept before eviction
    /// (settings-owned, default 14; `0` = keep forever).
    pub async fn media_cache_retention_days(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(14, |s| s.media_cache_retention_days)
    }

    /// Anonymous access to the federated timeline (settings-owned,
    /// default off).
    pub async fn timeline_preview_federated(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.timeline_preview_federated)
    }

    /// Anonymous access to the local timeline (settings-owned, default on).
    pub async fn timeline_preview_local(&self) -> bool {
        self.setting_overrides()
            .await
            .is_none_or(|s| s.timeline_preview_local)
    }

    /// Anonymous access to hashtag timelines (settings-owned, default off).
    pub async fn timeline_preview_tag(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.timeline_preview_tag)
    }

    /// Anonymous access to search (settings-owned, default off).
    pub async fn public_search(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.public_search)
    }

    /// Anonymous access to the Trending web pages (settings-owned, default
    /// on).
    pub async fn anon_trends(&self) -> bool {
        self.setting_overrides().await.is_none_or(|s| s.anon_trends)
    }

    /// Anonymous access to the People directory web page (settings-owned,
    /// default on).
    pub async fn anon_directory(&self) -> bool {
        self.setting_overrides()
            .await
            .is_none_or(|s| s.anon_directory)
    }

    /// Whether anonymous visitors may widen the People directory to remote
    /// profiles (settings-owned, default off).
    pub async fn anon_directory_federated(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.anon_directory_federated)
    }

    /// Anonymous access to the Groups directory web page (settings-owned,
    /// default on).
    pub async fn anon_groups(&self) -> bool {
        self.setting_overrides().await.is_none_or(|s| s.anon_groups)
    }

    /// Whether the public and local timelines carry replies to other people
    /// (settings-owned, default off — Mastodon's `PublicFeed`).
    pub async fn public_timeline_replies(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.public_timeline_replies)
    }

    /// Whether the built-in web client merges repeated boosts of one post
    /// (settings-owned, default on). Presentation only — never the API.
    pub async fn boost_collapse(&self) -> bool {
        self.setting_overrides()
            .await
            .is_none_or(|s| s.boost_collapse)
    }

    /// How many posts above the rendered page boost collapse looks back
    /// (settings-owned, default 0 = within the page only).
    pub async fn boost_collapse_lookback(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(0, |s| s.boost_collapse_lookback)
    }

    /// Days an unused cached status translation is kept (settings-owned,
    /// default 30; `0` = keep forever).
    pub async fn translation_cache_retention_days(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(30, |s| s.translation_cache_retention_days)
    }

    /// Hard row cap for the persistent translation cache (settings-owned,
    /// default 200 000; `0` = uncapped).
    pub async fn translation_cache_max_rows(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(200_000, |s| s.translation_cache_max_rows)
    }

    /// Concurrent requests allowed toward the translation backend
    /// (settings-owned, default 2, floored at 1).
    pub async fn translation_backend_concurrency(&self) -> usize {
        let configured = self
            .setting_overrides()
            .await
            .map_or(2, |s| s.translation_backend_concurrency);
        usize::try_from(configured.max(1)).unwrap_or(1)
    }

    /// Per-account translations per hour that may reach the backend
    /// (settings-owned, default 60; `0` disables the limit).
    pub async fn translation_user_rate_limit_per_hour(&self) -> i32 {
        self.setting_overrides()
            .await
            .map_or(60, |s| s.translation_user_rate_limit_per_hour)
    }

    /// Whether cached translations from a different backend are re-translated
    /// (settings-owned, default off — serve them with stored attribution).
    pub async fn translation_refresh_on_provider_change(&self) -> bool {
        self.setting_overrides()
            .await
            .is_some_and(|s| s.translation_refresh_on_provider_change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A domain that cannot serve as a `WebAuthn` relying party
    /// reports an error — [`AppState::new`] threads this `Result` instead of
    /// asserting the config-time proof with a panic.
    #[test]
    fn webauthn_construction_is_fallible_not_panicking() {
        assert!(try_build_webauthn("plamenu.example").is_ok());
        assert!(try_build_webauthn("not a hostname").is_err());
        assert!(try_build_webauthn("").is_err());
    }

    #[test]
    fn inflight_guard_reclaims_slot_on_scope_exit() {
        let cache = TranslationCache::default();
        {
            let _guard = cache.inflight_lock(1, "de");
            assert_eq!(cache.inflight_len(), 1);
        }
        assert_eq!(cache.inflight_len(), 0);
    }

    #[test]
    fn inflight_guard_reclaims_slot_on_early_return_and_success() {
        // Mirrors `translation::translate`: `inflight` is a function-scoped
        // guard and the miss path lives in an inner block that can return from
        // the whole function via `?`. Both the error and success exits must
        // reclaim the slot (finding #20 — the old `finish_inflight` tail call
        // was skipped by those `?` early returns).
        fn translate_like(cache: &TranslationCache, fail: bool) -> Result<(), &'static str> {
            let _guard = cache.inflight_lock(7, "fr");
            assert_eq!(cache.inflight_len(), 1);
            {
                if fail {
                    return Err("backend error");
                }
            }
            Ok(())
        }

        let cache = TranslationCache::default();
        assert!(translate_like(&cache, true).is_err());
        assert_eq!(cache.inflight_len(), 0, "error path must reclaim the slot");
        assert!(translate_like(&cache, false).is_ok());
        assert_eq!(
            cache.inflight_len(),
            0,
            "success path must reclaim the slot"
        );
    }

    /// A burst of concurrent misses (cold cache, then after `invalidate`) must
    /// refresh the settings row exactly once each — the single-flight guarantee
    /// that replaces the old stampede where every misser queried independently.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn concurrent_misses_refresh_the_settings_once(pool: PgPool) {
        let cache = Arc::new(SettingsCache::default());

        let burst = |cache: Arc<SettingsCache>, pool: PgPool| async move {
            let tasks: Vec<_> = (0..24)
                .map(|_| {
                    let cache = Arc::clone(&cache);
                    let pool = pool.clone();
                    tokio::spawn(async move { cache.get(&pool).await.map(|_| ()) })
                })
                .collect();
            for task in tasks {
                task.await.unwrap().unwrap();
            }
        };

        // Cold cache: 24 racing callers, one query.
        burst(Arc::clone(&cache), pool.clone()).await;
        assert_eq!(cache.fetch_count(), 1, "cold burst should query once");

        // A fresh value is served from cache without another query.
        cache.get(&pool).await.unwrap();
        assert_eq!(cache.fetch_count(), 1);

        // Invalidation forces a miss; the next burst still refreshes only once.
        cache.invalidate();
        burst(Arc::clone(&cache), pool.clone()).await;
        assert_eq!(
            cache.fetch_count(),
            2,
            "post-invalidate burst should query once"
        );
    }

    /// An entry served past its TTL triggers exactly one refresh, proving the
    /// expiry path (not just `invalidate`) is single-flight.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn expired_entries_trigger_a_single_refresh(pool: PgPool) {
        let cache = SettingsCache::new(Duration::from_millis(20));
        cache.get(&pool).await.unwrap();
        assert_eq!(cache.fetch_count(), 1);
        // Still within the TTL: no new query.
        cache.get(&pool).await.unwrap();
        assert_eq!(cache.fetch_count(), 1);
        // Past the TTL: one refresh.
        tokio::time::sleep(Duration::from_millis(40)).await;
        cache.get(&pool).await.unwrap();
        assert_eq!(cache.fetch_count(), 2, "an expired entry refreshes");
    }

    /// A refresh that fails serves the last-known settings instead of an error
    /// operator knobs are slow-moving, and an instance-wide
    /// error storm during database pressure is the worse outcome. Only a cache
    /// that has never loaded propagates the failure.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn failed_refresh_serves_last_known_settings(pool: PgPool) {
        let cache = SettingsCache::new(Duration::from_millis(20));
        let loaded = cache.get(&pool).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;

        // The entry is expired and the database is gone: the stale value is
        // served, repeatedly, rather than an error.
        pool.close().await;
        for _ in 0..3 {
            let stale = cache.get(&pool).await.unwrap();
            assert_eq!(stale.site_title, loaded.site_title);
        }

        // A cache that never loaded has nothing to fall back to.
        let cold = SettingsCache::new(Duration::from_millis(20));
        assert!(cold.get(&pool).await.is_err());
    }
}
