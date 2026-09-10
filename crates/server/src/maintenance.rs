//! Background maintenance & lifecycle jobs (M35) — the daily/hourly schedulers
//! Mastodon runs that Plamenu previously had no analogue for, folded into one
//! worker on a single hourly tick:
//!
//! * **Daily** (`UserCleanupScheduler`, `IpCleanupScheduler`, `VacuumScheduler`): delete stalled
//!   unconfirmed sign-ups, forget aged IPs, prune the sign-in log, and vacuum spent OAuth
//!   tokens/grants and orphaned preview cards.
//! * **Hourly** (`AutoCloseRegistrationsScheduler`): if open registration has been left unattended
//!   by every moderator for a week, fall back to approval mode so a dormant instance can't be
//!   overrun.
//! * **Every 6h** (`SoftwareUpdateCheckScheduler`): poll the operator's release feed for newer
//!   versions (opt-in — no feed configured, no check).
//!
//! Suspended accounts also receive Mastodon's 30-day reversible grace period;
//! the daily pass permanently purges requests that have reached their due date.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use plamenu_db::instance_settings::{self, RegistrationsMode, SettingsUpdate};
use plamenu_db::role::permission;
use plamenu_db::{maintenance, software_update};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::AppState;

/// Unconfirmed sign-ups are swept after this long — Mastodon's
/// `UserCleanupScheduler::UNCONFIRMED_ACCOUNTS_MAX_AGE_DAYS`.
const UNCONFIRMED_MAX_AGE_DAYS: i32 = 7;
/// Open registration reverts to approval mode when no moderator has signed in
/// within this window — Mastodon's `OPEN_REGISTRATIONS_MODERATOR_THRESHOLD`
/// (1 week + the 24h sign-in-tracking granularity).
const MODERATOR_IDLE_DAYS: i64 = 8;

/// Soft-deleted "stub" statuses (kept to thread a deleted middle post) are
/// hard-wiped once they become leaves and are older than this — aligned with
/// the tombstone window so the AP object route answers `410 Gone`
/// continuously. `STUB_PRUNE_BUDGET` caps how many one daily pass reaps.
const STUB_PRUNE_AFTER_DAYS: i64 = 7;
const STUB_PRUNE_BUDGET: i64 = 2_000;
const SUSPENSION_PURGE_BATCH: i64 = 100;

/// A hosted Webxdc session with no durable activity for this long is closed;
/// closure remains visible in the ended-session archive during the grace
/// period below.
pub const WEBXDC_ACTIVE_IDLE_DAYS: i64 = 90;
/// Closed Webxdc application state is purged after this grace period.
pub const WEBXDC_CLOSED_RETENTION_DAYS: i64 = 30;
/// Retain a deleted local coordinator's signing account for at least this
/// long, and always until its queued deliveries have drained.
pub const WEBXDC_COORDINATOR_RETENTION_DAYS: i64 = 1;
const WEBXDC_LIFECYCLE_BATCH: i64 = 100;

/// The worker ticks hourly; these are the tick counts between the less-frequent
/// jobs.
const TICKS_PER_DAILY_RUN: u64 = 24;
const TICKS_PER_UPDATE_CHECK: u64 = 6;

/// Rows fetched per keyset page while backfilling stored file sizes (finding
/// #54). Small enough that a class missing millions of sizes never lands in
/// one Rust vector, large enough that the per-page round trip is amortized.
const BACKFILL_PAGE: i64 = 500;

/// The daily cleanup + vacuum pass. Every operation is isolated: a failure is
/// logged and the rest still run (Mastodon's `VacuumScheduler` rescues each
/// operation individually).
pub async fn run_daily(state: &AppState) {
    let retention = state.ip_retention_days().await;

    log_op(
        "unconfirmed accounts",
        maintenance::delete_stale_unconfirmed(&state.pool, UNCONFIRMED_MAX_AGE_DAYS).await,
    );
    purge_expired_suspensions(state).await;
    log_op(
        "sign-in log",
        maintenance::prune_login_activities(&state.pool, retention).await,
    );
    log_op(
        "stale IPs",
        maintenance::scrub_stale_ips(&state.pool, retention).await,
    );
    log_op(
        "leaf stubs",
        plamenu_db::status::prune_leaf_stubs(
            &state.pool,
            OffsetDateTime::now_utc() - time::Duration::days(STUB_PRUNE_AFTER_DAYS),
            STUB_PRUNE_BUDGET,
        )
        .await,
    );
    log_op("oauth", maintenance::vacuum_oauth(&state.pool).await);
    log_op(
        "orphan preview cards",
        maintenance::vacuum_orphan_preview_cards(
            &state.pool,
            state.media_cache_retention_days().await,
        )
        .await,
    );
}

async fn purge_expired_suspensions(state: &AppState) {
    let ids = match plamenu_db::account::expired_suspension_ids(&state.pool, SUSPENSION_PURGE_BATCH)
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error, "expired suspension scan failed");
            return;
        }
    };
    let mut purged = 0_u64;
    for account_id in ids {
        let snapshot = match plamenu_db::account::find_by_id(&state.pool, account_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::error!(%error, account_id, "expired suspension load failed");
                continue;
            }
        };
        let reach = if snapshot
            .as_ref()
            .is_some_and(plamenu_db::account::Account::is_local)
        {
            plamenu_db::account::reach_inboxes(&state.pool, account_id)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        match plamenu_db::account::purge_expired_suspension(&state.pool, account_id).await {
            Ok(true) => {
                purged += 1;
                if let Some(account) = snapshot.filter(plamenu_db::account::Account::is_local) {
                    let delete =
                        plamenu_ap::activity::delete_actor(&state.config.domain, &account.username);
                    if let Err(error) = plamenu_db::job::enqueue_many(
                        &state.pool,
                        account.id,
                        &reach,
                        &delete,
                        false,
                    )
                    .await
                    {
                        tracing::error!(%error, account_id, "permanent suspension Delete fan-out failed");
                    }
                }
            }
            Ok(false) => {}
            Err(error) => tracing::error!(%error, account_id, "expired suspension purge failed"),
        }
    }
    if purged > 0 {
        tracing::info!(count = purged, "maintenance purged expired suspensions");
    }
}

/// The hourly translation-cache sweep: retention first, then the row cap.
/// Hourly rather than daily so a runaway
/// burst can't sit far above the cap for long.
pub async fn prune_translation_cache(state: &AppState) {
    log_op(
        "stale translations",
        plamenu_db::status_translation::prune_unused(
            &state.pool,
            state.translation_cache_retention_days().await,
        )
        .await,
    );
    log_op(
        "translation row cap",
        plamenu_db::status_translation::enforce_cap(
            &state.pool,
            state.translation_cache_max_rows().await,
        )
        .await,
    );
}

/// Closes idle locally coordinated Webxdc sessions and purges sessions whose
/// closure grace period elapsed. Local coordinators emit a durable `Delete`;
/// remote sessions are only local caches and can be dropped directly.
pub async fn webxdc_lifecycle(state: &AppState) {
    let now = OffsetDateTime::now_utc();
    let candidates = match plamenu_db::webxdc::lifecycle_candidates(
        &state.pool,
        now - time::Duration::days(WEBXDC_ACTIVE_IDLE_DAYS),
        now - time::Duration::days(WEBXDC_CLOSED_RETENTION_DAYS),
        WEBXDC_LIFECYCLE_BATCH,
    )
    .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::error!(%error, "Webxdc lifecycle candidate scan failed");
            return;
        }
    };
    for session in candidates {
        let result = if session.ended() {
            match plamenu_db::account::find_by_id(&state.pool, session.account_id).await {
                Ok(Some(coordinator)) if coordinator.is_local() => {
                    crate::webxdc::delete_local(state, &session)
                        .await
                        .map(|()| "purged local")
                }
                Ok(Some(_)) => plamenu_db::webxdc::discard_remote_cache(&state.pool, session.id)
                    .await
                    .map(|_| "purged remote cache")
                    .map_err(crate::error::ApiError::from),
                Ok(None) => Err(crate::error::ApiError::NotFound),
                Err(error) => Err(crate::error::ApiError::from(error)),
            }
        } else {
            crate::webxdc::close_local(state, &session)
                .await
                .map(|_| "closed idle")
        };
        match result {
            Ok(action) => {
                tracing::info!(session_id = session.id, action, "Webxdc lifecycle applied");
            }
            Err(error) => tracing::error!(
                session_id = session.id,
                error = %error.chain(),
                "Webxdc lifecycle action failed"
            ),
        }
    }
    log_op(
        "Webxdc coordinator accounts",
        plamenu_db::webxdc::prune_tombstoned_coordinator_accounts(
            &state.pool,
            now - time::Duration::days(WEBXDC_COORDINATOR_RETENTION_DAYS),
            WEBXDC_LIFECYCLE_BATCH,
        )
        .await,
    );
}

fn log_op(op: &str, result: Result<u64, plamenu_db::DbError>) {
    match result {
        Ok(0) => {}
        Ok(count) => tracing::info!(op, count, "maintenance swept rows"),
        Err(error) => tracing::error!(%error, op, "maintenance operation failed"),
    }
}

/// Reverts open registration to approval mode when no moderator has been active
/// for a week — Mastodon's `AutoCloseRegistrationsScheduler`. A no-op unless
/// registration is currently `open`.
pub async fn auto_close_registrations(state: &AppState) {
    let settings = match instance_settings::get(&state.pool).await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "auto-close registrations: settings read failed");
            return;
        }
    };
    if settings.registrations_mode() != RegistrationsMode::Open {
        return;
    }
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(MODERATOR_IDLE_DAYS);
    match maintenance::active_moderator_since(
        &state.pool,
        permission::MANAGE_REPORTS,
        permission::ADMINISTRATOR,
        cutoff,
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            let update = SettingsUpdate {
                registrations_mode: RegistrationsMode::Approved,
                ..settings.as_update()
            };
            match instance_settings::save(&state.pool, update).await {
                Ok(_) => {
                    // After `save` — it clears the stamp (operator-acknowledge
                    // semantics), so the order matters.
                    if let Err(error) =
                        instance_settings::mark_registrations_auto_closed(&state.pool).await
                    {
                        tracing::error!(%error, "auto-close registrations: stamp failed");
                    }
                    state.settings_cache.invalidate();
                    tracing::warn!(
                        "no moderator active in {MODERATOR_IDLE_DAYS} days; \
                         open registration reverted to approval mode"
                    );
                }
                Err(error) => tracing::error!(%error, "auto-close registrations: save failed"),
            }
        }
        Err(error) => tracing::error!(%error, "auto-close registrations: activity check failed"),
    }
}

/// One release entry from the configured update feed.
#[derive(Debug, Deserialize)]
struct FeedEntry {
    version: String,
    #[serde(default)]
    urgent: bool,
    #[serde(default = "default_release_type", rename = "type")]
    release_type: String,
    #[serde(default)]
    release_notes: String,
}

fn default_release_type() -> String {
    "patch".to_owned()
}

/// Polls the operator's release feed and stores the versions ahead of the
/// running one — Mastodon's `SoftwareUpdateCheckService`. Disabled (and a
/// no-op) unless `update_check_url` is configured.
pub async fn check_software_updates(state: &AppState) {
    let Some(url) = state.config.update_check_url.as_deref() else {
        return;
    };
    let page = match state.federation.fetch_page(url, "application/json").await {
        Ok(page) => page,
        Err(error) => {
            tracing::warn!(error = %crate::error::ErrorChain(&error), "software update check fetch failed");
            return;
        }
    };
    let entries: Vec<FeedEntry> = match serde_json::from_str(&page.body) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "software update feed parse failed");
            return;
        }
    };
    let updates: Vec<software_update::NewSoftwareUpdate> = entries
        .into_iter()
        .filter(|entry| version_is_newer(&entry.version, crate::PACKAGE_VERSION))
        .map(|entry| software_update::NewSoftwareUpdate {
            version: entry.version,
            urgent: entry.urgent,
            release_type: entry.release_type,
            release_notes: entry.release_notes,
        })
        .collect();
    if let Err(error) = software_update::replace_with(&state.pool, &updates).await {
        tracing::error!(%error, "software update store failed");
    } else if !updates.is_empty() {
        tracing::info!(count = updates.len(), "software updates available");
    }
}

/// Whether release `candidate` is strictly newer than `current`, comparing the
/// leading dotted numeric components (`4.6.10` > `4.6.9`). A pre-release suffix
/// (`-rc1`) is ignored; anything that fails to parse is treated as not newer,
/// so a malformed feed can never raise a false update banner.
fn version_is_newer(candidate: &str, current: &str) -> bool {
    match (numeric_version(candidate), numeric_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn numeric_version(version: &str) -> Option<Vec<u64>> {
    let core = version.trim_start_matches('v').split(['-', '+']).next()?;
    let parts: Vec<u64> = core
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// Process-wide single-flight flag for the stored-size backfill. The sweep
/// stats every stored file, so overlapping runs from repeated admin-button
/// clicks (finding #54) would multiply the disk reads and per-row updates for
/// no gain. The CLI runs in its own process, so its flag is independent.
static BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);

/// RAII holder that clears [`BACKFILL_RUNNING`] on drop — including an early
/// `?` return, a panic, or the spawned task ending — so a failed or aborted
/// sweep never wedges the flag on and blocks all later backfills.
pub struct BackfillGuard(());

impl Drop for BackfillGuard {
    fn drop(&mut self) {
        BACKFILL_RUNNING.store(false, Ordering::Release);
    }
}

/// Claims the single-flight slot, returning `None` when a backfill is already
/// running. Hold the returned guard for the whole sweep (move it into the
/// spawned task); dropping it frees the slot.
#[must_use]
pub fn try_begin_backfill() -> Option<BackfillGuard> {
    BACKFILL_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
        .then_some(BackfillGuard(()))
}

/// Stats each stored file whose byte size is still unknown and records it, so
/// the admin storage metrics (`space_usage`, `instance_media_attachments`)
/// account for media stored before sizes were tracked. One-shot migration
/// aid, shared by `plamenu media backfill-sizes` and the admin console's
/// maintenance button (O5). The web trigger claims [`try_begin_backfill`]
/// before spawning this; the CLI is alone in its process. Returns
/// `(filled, skipped)`; unreadable files are logged and skipped.
pub async fn backfill_stored_sizes(state: &AppState) -> Result<(u64, u64), crate::error::ApiError> {
    /// Reads a stored file's byte length without buffering its contents
    /// (finding #54: a multi-GB feature video must not be loaded into memory
    /// just to stat it), or `None` (with a warning) when the file is missing.
    async fn stored_size(state: &AppState, file_name: &str, skipped: &mut u64) -> Option<i64> {
        match state.media.open(file_name).await {
            Ok(opened) => Some(i64::try_from(opened.len).unwrap_or(i64::MAX)),
            Err(error) => {
                tracing::warn!(file_name, %error, "size backfill skipped an unreadable file");
                *skipped += 1;
                None
            }
        }
    }

    let mut filled = 0_u64;
    let mut skipped = 0_u64;

    // Each class is walked in keyset-paged batches (finding #54): a class
    // missing sizes for millions of rows must never be materialized in one
    // vector. The id cursor advances past every row we fetch — including one
    // whose file we had to skip — so the walk terminates, and because a filled
    // row leaves the `… IS NULL` predicate the sweep is idempotent and resumes
    // from the lowest still-missing id after a crash or restart.
    let mut after = 0_i64;
    loop {
        let page =
            plamenu_db::backfill::media_missing_sizes_page(&state.pool, after, BACKFILL_PAGE)
                .await?;
        let Some(last) = page.last() else { break };
        after = last.id;
        // The file stats stay per row (they read the store, not the
        // database); the measured page is written back in one statement.
        let mut ids = Vec::new();
        let mut file_sizes = Vec::new();
        let mut thumbnail_sizes: Vec<Option<i64>> = Vec::new();
        for target in page {
            let Some(file_size) = stored_size(state, &target.file_name, &mut skipped).await else {
                continue;
            };
            let thumbnail_file_size = match &target.thumb_file_name {
                Some(name) => stored_size(state, name, &mut skipped).await,
                None => None,
            };
            ids.push(target.id);
            file_sizes.push(file_size);
            thumbnail_sizes.push(thumbnail_file_size);
        }
        filled += ids.len() as u64;
        plamenu_db::media::set_file_sizes_many(&state.pool, &ids, &file_sizes, &thumbnail_sizes)
            .await?;
    }

    after = 0;
    loop {
        let page =
            plamenu_db::backfill::avatars_missing_sizes_page(&state.pool, after, BACKFILL_PAGE)
                .await?;
        let Some(last) = page.last() else { break };
        after = last.id;
        let mut ids = Vec::new();
        let mut sizes = Vec::new();
        for target in page {
            if let Some(size) = stored_size(state, &target.file_name, &mut skipped).await {
                ids.push(target.id);
                sizes.push(size);
            }
        }
        filled += ids.len() as u64;
        plamenu_db::backfill::set_avatar_sizes(&state.pool, &ids, &sizes).await?;
    }

    after = 0;
    loop {
        let page =
            plamenu_db::backfill::headers_missing_sizes_page(&state.pool, after, BACKFILL_PAGE)
                .await?;
        let Some(last) = page.last() else { break };
        after = last.id;
        let mut ids = Vec::new();
        let mut sizes = Vec::new();
        for target in page {
            if let Some(size) = stored_size(state, &target.file_name, &mut skipped).await {
                ids.push(target.id);
                sizes.push(size);
            }
        }
        filled += ids.len() as u64;
        plamenu_db::backfill::set_header_sizes(&state.pool, &ids, &sizes).await?;
    }

    after = 0;
    loop {
        let page =
            plamenu_db::backfill::emoji_missing_sizes_page(&state.pool, after, BACKFILL_PAGE)
                .await?;
        let Some(last) = page.last() else { break };
        after = last.id;
        let mut ids = Vec::new();
        let mut sizes = Vec::new();
        for target in page {
            if let Some(size) = stored_size(state, &target.file_name, &mut skipped).await {
                ids.push(target.id);
                sizes.push(size);
            }
        }
        filled += ids.len() as u64;
        plamenu_db::backfill::set_emoji_sizes(&state.pool, &ids, &sizes).await?;
    }

    Ok((filled, skipped))
}

/// Runs the maintenance schedules until the process exits. Ticks hourly, with a
/// short startup delay so boot isn't a work storm; the daily and update-check
/// jobs fire on their own multiples of the tick.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("maintenance worker started");
        // Settle before the first pass so startup traffic isn't fighting a sweep.
        if !crate::workers::pause(&state, Duration::from_mins(5)).await {
            return;
        }
        let mut tick: u64 = 0;
        loop {
            auto_close_registrations(&state).await;
            prune_translation_cache(&state).await;
            webxdc_lifecycle(&state).await;
            log_op(
                "rate-limit windows",
                plamenu_db::rate_limit::sweep_expired(&state.pool).await,
            );
            if tick.is_multiple_of(TICKS_PER_UPDATE_CHECK) {
                check_software_updates(&state).await;
            }
            if tick.is_multiple_of(TICKS_PER_DAILY_RUN) {
                run_daily(&state).await;
            }
            tick = tick.wrapping_add(1);
            if !crate::workers::pause(&state, Duration::from_hours(1)).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{numeric_version, version_is_newer};

    #[test]
    fn newer_versions_detected() {
        assert!(version_is_newer("4.6.3", "4.6.2"));
        assert!(version_is_newer("4.6.10", "4.6.9")); // numeric, not lexical
        assert!(version_is_newer("5.0.0", "4.9.9"));
        assert!(version_is_newer("v1.2.0-rc1", "1.1.0")); // prefix + pre-release stripped
    }

    #[test]
    fn same_or_older_or_unparseable_is_not_newer() {
        assert!(!version_is_newer("4.6.2", "4.6.2"));
        assert!(!version_is_newer("4.6.1", "4.6.2"));
        assert!(!version_is_newer("nightly", "4.6.2"));
        assert!(!version_is_newer("4.6.2", "garbage"));
    }

    #[test]
    fn numeric_version_parses_components() {
        assert_eq!(numeric_version("4.6.10"), Some(vec![4, 6, 10]));
        assert_eq!(numeric_version("v2.0"), Some(vec![2, 0]));
        assert_eq!(numeric_version("1.2.3-beta"), Some(vec![1, 2, 3]));
        assert_eq!(numeric_version("abc"), None);
        assert_eq!(numeric_version(""), None);
    }

    #[test]
    fn backfill_single_flight() {
        // First claim wins; a second is refused while it is held.
        let first = super::try_begin_backfill().expect("first claim succeeds");
        assert!(
            super::try_begin_backfill().is_none(),
            "a concurrent backfill claim is refused while one runs"
        );
        // Dropping the guard frees the slot for the next run.
        drop(first);
        let again = super::try_begin_backfill().expect("slot is free after the guard drops");
        drop(again);
    }
}
