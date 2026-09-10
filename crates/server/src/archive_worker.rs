//! The account-archive worker — Mastodon's `BackupWorker`. It claims
//! `scheduled` archives, builds each zip through [`crate::archive::build`],
//! stores it in the media store, keeps only the newest archive per account,
//! and e-mails a download link. It also sweeps archives past the retention
//! window, like Mastodon's `Vacuum::BackupsVacuum`.

use std::time::Duration;

use plamenu_db::account::{self, Account};
use plamenu_db::archive::{self, AccountArchive};
use plamenu_db::{PgPool, user};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::AppState;
use crate::storage::MediaStore;
use crate::worker::RetentionSchedule;

/// How many archives one pass claims. Builds are heavy (all of an account's
/// statuses plus media), so the batch is small.
const BATCH_SIZE: i64 = 2;
/// How long the worker naps when there is nothing to do.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// The retention sweep runs on this fixed cadence regardless of queue load, so
/// a continuously busy queue can no longer starve it.
const SWEEP_INTERVAL: Duration = Duration::from_mins(10);
/// Archives older than this are swept — Mastodon's backup retention period.
pub const RETENTION_DAYS: i32 = 7;
/// A build lease long enough to exceed any real archive build; an `in_progress`
/// row idle longer than this is treated as a crashed worker and requeued.
const BUILD_LEASE: Duration = Duration::from_hours(1);
/// How often the worker checks for `in_progress` archives stranded by a crash.
/// Starts due, so a restarted process reclaims immediately.
const RECLAIM_INTERVAL: Duration = Duration::from_mins(5);

/// Returns archives stranded `in_progress` by a crashed worker to `scheduled`
/// so this (or another) process rebuilds them.
async fn reclaim_crashed(pool: &PgPool) {
    match archive::reclaim_stale(pool, BUILD_LEASE).await {
        Ok(0) => {}
        Ok(requeued) => tracing::warn!(requeued, "requeued crashed account archive builds"),
        Err(error) => tracing::error!(%error, "reclaiming crashed archive builds failed"),
    }
}

/// Claims and builds one batch of scheduled archives; returns how many were
/// claimed (0 = nothing scheduled right now).
pub async fn run_due(state: &AppState) -> u64 {
    let archives = match archive::claim_scheduled(&state.pool, BATCH_SIZE).await {
        Ok(archives) => archives,
        Err(error) => {
            tracing::error!(%error, "failed to claim account archives");
            return 0;
        }
    };
    let claimed = archives.len() as u64;
    for archive in archives {
        process(state, &archive).await;
    }
    claimed
}

async fn process(state: &AppState, archive: &AccountArchive) {
    let account = match account::find_by_id(&state.pool, archive.account_id).await {
        Ok(Some(account)) => account,
        // The account vanished mid-flight (deleted); drop the request.
        Ok(None) => {
            let _ = archive::delete_by_id(&state.pool, archive.id).await;
            return;
        }
        Err(error) => {
            tracing::error!(archive = archive.id, %error, "archive: account lookup failed");
            return;
        }
    };
    tracing::info!(
        archive = archive.id,
        account = account.id,
        "building account archive"
    );
    if let Err(error) = build_and_store(state, archive, &account).await {
        // A build error is almost always permanent (a missing file, a
        // serialization bug). Drop the request so the user can retry without
        // waiting out the rate limit, matching Mastodon's retry-exhausted path.
        tracing::error!(archive = archive.id, error = %error.chain(), "account archive failed");
        let _ = archive::delete_by_id(&state.pool, archive.id).await;
    }
}

/// Builds the zip, stores it, finalizes the row, prunes superseded archives and
/// notifies the user.
async fn build_and_store(
    state: &AppState,
    archive: &AccountArchive,
    account: &Account,
) -> Result<(), crate::error::ApiError> {
    // The build streams the ZIP into a temp file under the media root; move it
    // into the store by rename rather than buffering its bytes.
    let temp = crate::archive::build(state, account).await?;
    let size = i64::try_from(
        tokio::fs::metadata(temp.path())
            .await
            .map_err(|e| crate::error::ApiError::Internal(Box::new(e)))?
            .len(),
    )
    .unwrap_or(i64::MAX);
    let file_name = format!("archive-{}.zip", archive.id);
    state
        .media
        .put_file(&file_name, temp.path())
        .await
        .map_err(|e| crate::error::ApiError::Internal(Box::new(e)))?;
    // Finish only if the row still exists. Account deletion (self-delete purge or
    // admin hard-delete) removes the archive row while a build is in flight (QC
    // audit #56); a conditional finish that returns `false` means we lost that
    // race, so delete the object we just wrote — leaving no untracked private ZIP
    // — and stop, since there is no row to prune or notify for.
    if !archive::mark_finished(&state.pool, archive.id, &file_name, size).await? {
        if let Err(error) = state.media.delete(&file_name).await {
            tracing::warn!(
                %error,
                file_name = %file_name,
                "archive: removing object after a lost deletion race failed"
            );
        }
        return Ok(());
    }

    // Keep only this archive; delete the account's older ones and their files.
    match archive::delete_superseded(&state.pool, account.id, archive.id).await {
        Ok(stale_files) => remove_files(state, &stale_files).await,
        Err(error) => tracing::error!(%error, "archive: pruning superseded archives failed"),
    }

    notify(state, account, archive.id).await;
    Ok(())
}

/// E-mails the download link when the account has a confirmed e-mail and a
/// relay is configured (both optional in Plamenu).
async fn notify(state: &AppState, account: &Account, archive_id: i64) {
    if !crate::mailer::enabled(state) {
        return;
    }
    let user = match user::find_by_account_id(&state.pool, account.id).await {
        Ok(Some(user)) => user,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, "archive: user lookup for notification failed");
            return;
        }
    };
    let Some(email) = user.email else { return };
    let domain = &state.config.domain;
    // The owner's stored locale: the mail is read long after the request.
    let locale = match crate::web::i18n::Locale::for_user(&state.pool, user.id).await {
        Ok(locale) => locale,
        Err(error) => {
            tracing::error!(%error, "archive: locale lookup for notification failed");
            crate::web::i18n::Locale::default()
        }
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("domain", domain.as_str());
    args.set("days", RETENTION_DAYS);
    let subject = locale.plain_with("email-archive-subject", &args);
    let body = format!(
        "{intro}\n\n\
         https://{domain}/settings/archive/{archive_id}/download\n\n\
         {outro}\n",
        intro = locale.plain("email-archive-intro"),
        outro = locale.plain_with("email-archive-outro", &args),
    );
    if let Err(error) = crate::mailer::enqueue(state, &email, &subject, &body).await {
        tracing::error!(%error, "archive: queueing ready notification failed");
    }
}

/// Deletes stored zip files, ignoring missing ones.
async fn remove_files(state: &AppState, file_names: &[String]) {
    for name in file_names {
        if let Err(error) = state.media.delete(name).await {
            tracing::warn!(%error, file_name = %name, "archive: removing stored file failed");
        }
    }
}

/// The outcome of one retention sweep, for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Archives whose stored file and row were both removed.
    pub swept: u64,
    /// Archives kept because their stored file could not be removed this pass;
    /// the next sweep retries them.
    pub retained_after_failure: u64,
}

/// Removes archives past the retention window and their stored files. The
/// private zip is deleted from the store *before* its row, so a transient store
/// failure keeps the row for the next sweep to retry instead of orphaning the
/// file with no database reference. Missing files are not an
/// error, so a row whose file is already gone is still removed. Called on a
/// fixed cadence via [`RetentionSchedule`] regardless of queue load.
pub async fn sweep(pool: &PgPool, media: &dyn MediaStore) -> SweepReport {
    let stale = match archive::stale(pool, RETENTION_DAYS).await {
        Ok(stale) => stale,
        Err(error) => {
            tracing::error!(%error, "stale archive sweep: selecting stale archives failed");
            return SweepReport::default();
        }
    };
    let mut removable = Vec::with_capacity(stale.len());
    let mut retained_after_failure = 0;
    for archive in &stale {
        match &archive.file_name {
            // Remove the private zip before dropping its only DB reference.
            Some(file) => match media.delete(file).await {
                Ok(()) => removable.push(archive.id),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        file_name = %file,
                        "stale archive sweep: removing stored file failed; keeping row to retry"
                    );
                    retained_after_failure += 1;
                }
            },
            // Never finished — no file to remove.
            None => removable.push(archive.id),
        }
    }
    let swept = match archive::delete_by_ids(pool, &removable).await {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(%error, "stale archive sweep: deleting swept rows failed");
            0
        }
    };
    report_sweep(pool, swept, retained_after_failure).await;
    SweepReport {
        swept,
        retained_after_failure,
    }
}

/// Emits the retention observability signal — swept/retained counts plus the
/// oldest surviving archive's age (the "oldest retained object" / "last
/// successful sweep" metrics). The completed pass is also
/// recorded durably in `retention_sweeps`, which the admin dashboard reads, so
/// the signal survives log rotation and is queryable. Logged at info when it
/// changed anything, otherwise a debug heartbeat so operators can still
/// confirm the sweep is running.
async fn report_sweep(pool: &PgPool, swept: u64, retained_after_failure: u64) {
    if let Err(error) =
        plamenu_db::retention_sweep::record(pool, "account archives", swept, retained_after_failure)
            .await
    {
        tracing::error!(%error, "failed to record archive retention sweep");
    }
    let oldest_retained_secs = archive::oldest_age_seconds(pool).await.unwrap_or(None);
    if swept > 0 || retained_after_failure > 0 {
        tracing::info!(
            swept,
            retained_after_failure,
            oldest_retained_secs,
            "swept stale account archives"
        );
    } else {
        tracing::debug!(
            oldest_retained_secs,
            "account archive retention sweep: nothing stale"
        );
    }
}

/// Spawns the archive worker: a fast claim loop with a time-driven retention
/// sweep. The sweep is checked every iteration — under load as well as when idle
/// — so a permanently busy queue can no longer starve retention;
/// it runs once at startup, then every [`SWEEP_INTERVAL`].
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("archive worker started");
        let mut retention = RetentionSchedule::new(SWEEP_INTERVAL);
        let mut reclaim = RetentionSchedule::new(RECLAIM_INTERVAL);
        loop {
            // Requeue builds a crashed worker left `in_progress`,
            // at startup and on a fixed cadence thereafter.
            if reclaim.due(Instant::now()) {
                reclaim_crashed(&state.pool).await;
            }
            let claimed = run_due(&state).await;
            if retention.due(Instant::now()) {
                sweep(&state.pool, state.media.as_ref()).await;
            }
            if claimed == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use plamenu_db::account::{self, NewLocalAccount};

    use super::*;
    use crate::federation::BoxFuture;
    use crate::storage::{MediaRead, MemoryStore};

    async fn seed_account(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn age_days(pool: &PgPool, id: i64, days: i32) {
        sqlx::query!(
            "UPDATE account_archives SET created_at = now() - make_interval(days => $2) WHERE id = $1",
            id,
            days,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// The retention sweep removes archives past the window *and* their stored
    /// files, while leaving in-window archives and still-pending (`scheduled`)
    /// work untouched — so a permanently non-empty queue neither blocks nor
    /// loses retention. This complements the `RetentionSchedule`
    /// unit test, which proves the sweep is time-driven rather than idle-gated.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn sweep_removes_expired_archives_and_their_files(pool: PgPool) {
        let alice = seed_account(&pool, "alice").await;
        let media = MemoryStore::default();

        // A finished archive aged past the 7-day window, with a stored file.
        let stale = archive::create(&pool, alice).await.unwrap();
        archive::mark_finished(&pool, stale.id, "stale.zip", 3)
            .await
            .unwrap();
        media
            .put("stale.zip", b"PK\x03\x04".to_vec())
            .await
            .unwrap();
        age_days(&pool, stale.id, 8).await;

        // A freshly finished archive (in-window) and a still-`scheduled`
        // request (the "non-empty queue") must both survive.
        let fresh = archive::create(&pool, alice).await.unwrap();
        archive::mark_finished(&pool, fresh.id, "fresh.zip", 3)
            .await
            .unwrap();
        media
            .put("fresh.zip", b"PK\x03\x04".to_vec())
            .await
            .unwrap();
        let pending = archive::create(&pool, seed_account(&pool, "bob").await)
            .await
            .unwrap();

        let report = sweep(&pool, &media).await;
        assert_eq!(
            report,
            SweepReport {
                swept: 1,
                retained_after_failure: 0
            }
        );

        // The stale row and its stored file are both gone.
        assert!(
            archive::find_for_account(&pool, alice, stale.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(media.get("stale.zip").await.is_err());
        // The in-window archive, its file, and the pending request all survive.
        assert!(
            archive::find_for_account(&pool, alice, fresh.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(media.get("fresh.zip").await.is_ok());
        assert_eq!(pending.state, "scheduled");
        assert!(
            archive::find_for_account(&pool, pending.account_id, pending.id)
                .await
                .unwrap()
                .is_some()
        );

        // A second sweep finds nothing stale.
        assert_eq!(sweep(&pool, &media).await, SweepReport::default());

        // Each completed pass recorded its status durably: the
        // admin dashboard reads this instead of grepping rotated logs.
        let statuses = plamenu_db::retention_sweep::statuses(&pool).await.unwrap();
        let recorded = statuses
            .iter()
            .find(|s| s.name == "account archives")
            .expect("sweep recorded durably");
        assert_eq!(recorded.last_swept, 0, "the latest (empty) pass is current");
        assert_eq!(recorded.last_retained, 0);
    }

    /// A store whose `delete` always fails, proving the sweep keeps the row when
    /// its private zip cannot be removed (the retry guarantee).
    /// `MemoryStore::delete` never errors, so the failure path needs this double.
    struct FailingDeleteStore;

    impl MediaStore for FailingDeleteStore {
        fn put(&self, _: &str, _: Vec<u8>) -> BoxFuture<'_, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn put_file(&self, _: &str, _: &Path) -> BoxFuture<'_, std::io::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn get(&self, _: &str) -> BoxFuture<'_, std::io::Result<Vec<u8>>> {
            Box::pin(async { Err(std::io::Error::other("unused")) })
        }
        fn open(&self, _: &str) -> BoxFuture<'_, std::io::Result<MediaRead>> {
            Box::pin(async { Err(std::io::Error::other("unused")) })
        }
        fn delete(&self, _: &str) -> BoxFuture<'_, std::io::Result<()>> {
            Box::pin(async { Err(std::io::Error::other("store unavailable")) })
        }
        fn list(&self) -> BoxFuture<'_, std::io::Result<Vec<String>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[sqlx::test(migrations = "../db/migrations")]
    async fn sweep_keeps_the_row_when_its_file_cannot_be_removed(pool: PgPool) {
        let alice = seed_account(&pool, "alice").await;
        let stale = archive::create(&pool, alice).await.unwrap();
        archive::mark_finished(&pool, stale.id, "stuck.zip", 3)
            .await
            .unwrap();
        age_days(&pool, stale.id, 8).await;

        // The file removal fails, so the row is kept — not deleted with its bytes
        // orphaned on disk — and the next sweep will retry it.
        let report = sweep(&pool, &FailingDeleteStore).await;
        assert_eq!(
            report,
            SweepReport {
                swept: 0,
                retained_after_failure: 1
            }
        );
        assert!(
            archive::find_for_account(&pool, alice, stale.id)
                .await
                .unwrap()
                .is_some(),
            "the stale archive row survives a failed file deletion for retry"
        );
    }
}
