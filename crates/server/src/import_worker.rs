//! The CSV-import worker — the DB-backed analogue of Mastodon's
//! `BulkImportWorker`. It claims confirmed (`scheduled`) imports, applies each
//! through [`crate::bulk_import::run`], and opportunistically sweeps stale
//! imports the way Mastodon's `Vacuum::ImportsVacuum` does.

use std::time::Duration;

use plamenu_db::PgPool;
use plamenu_db::bulk_import::{self, BulkImport};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::AppState;
use crate::worker::RetentionSchedule;

/// How many imports one pass claims. Imports run sequentially and can be
/// large, so the batch is small.
const BATCH_SIZE: i64 = 4;
/// How long the worker naps when there is nothing to do.
const IDLE_POLL: Duration = Duration::from_secs(2);
/// The retention sweep runs on this fixed cadence regardless of queue load, so
/// a continuously busy queue can no longer starve it.
const SWEEP_INTERVAL: Duration = Duration::from_mins(10);
/// A lease long enough to exceed any real import run; an `in_progress` row idle
/// longer than this is treated as a crashed worker and requeued.
const IMPORT_LEASE: Duration = Duration::from_hours(1);
/// How often the worker checks for imports stranded `in_progress` by a crash.
/// Starts due, so a restarted process reclaims immediately.
const RECLAIM_INTERVAL: Duration = Duration::from_mins(5);

/// Returns imports stranded `in_progress` by a crashed worker to `scheduled` so
/// they are finished rather than deleted unfinished by the weekly sweep (QC
/// audit #1). Re-running a resumed import is safe: imported rows were deleted as
/// they succeeded and the underlying actions are idempotent.
async fn reclaim_crashed(pool: &PgPool) {
    match bulk_import::reclaim_stale(pool, IMPORT_LEASE).await {
        Ok(0) => {}
        Ok(requeued) => tracing::warn!(requeued, "requeued crashed bulk imports"),
        Err(error) => tracing::error!(%error, "reclaiming crashed imports failed"),
    }
}

/// Claims and applies one batch of confirmed imports; returns how many were
/// claimed (0 = nothing scheduled right now).
pub async fn run_due(state: &AppState) -> u64 {
    let imports = match bulk_import::claim_scheduled(&state.pool, BATCH_SIZE).await {
        Ok(imports) => imports,
        Err(error) => {
            tracing::error!(%error, "failed to claim bulk imports");
            return 0;
        }
    };
    let claimed = imports.len() as u64;
    for import in imports {
        Box::pin(process(state, &import)).await;
    }
    claimed
}

async fn process(state: &AppState, import: &BulkImport) {
    tracing::info!(
        import = import.id,
        import_type = %import.import_type,
        rows = import.total_items,
        "applying bulk import"
    );
    if let Err(error) = Box::pin(crate::bulk_import::run(state, import)).await {
        // The import stays `in_progress`; the weekly stale sweep reclaims it.
        tracing::error!(import = import.id, error = %error.chain(), "bulk import failed");
    }
}

/// Removes imports past their retention windows — Mastodon's
/// `Vacuum::ImportsVacuum` (unconfirmed after 10 minutes, any import after a
/// week). Imports carry no media files, so a row delete is the whole unit of
/// cleanup. Returns how many rows were swept.
pub async fn sweep(pool: &PgPool) -> u64 {
    let swept = match bulk_import::delete_stale(pool).await {
        Ok(swept) => swept,
        Err(error) => {
            tracing::error!(%error, "stale import sweep failed");
            return 0;
        }
    };
    // The "oldest retained object" / "last successful sweep" retention signal
    // recorded durably in `retention_sweeps` for the admin
    // dashboard, then info when it removed rows, otherwise a debug heartbeat.
    if let Err(error) = plamenu_db::retention_sweep::record(pool, "bulk imports", swept, 0).await {
        tracing::error!(%error, "failed to record import retention sweep");
    }
    let oldest_retained_secs = bulk_import::oldest_age_seconds(pool).await.unwrap_or(None);
    if swept > 0 {
        tracing::info!(swept, oldest_retained_secs, "swept stale bulk imports");
    } else {
        tracing::debug!(
            oldest_retained_secs,
            "bulk import retention sweep: nothing stale"
        );
    }
    swept
}

/// Spawns the import worker: a fast claim loop with a time-driven retention
/// sweep. The sweep is checked every iteration — under load as well as when idle
/// — so a permanently busy queue can no longer starve retention;
/// it runs once at startup, then every [`SWEEP_INTERVAL`].
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!("import worker started");
        let mut retention = RetentionSchedule::new(SWEEP_INTERVAL);
        let mut reclaim = RetentionSchedule::new(RECLAIM_INTERVAL);
        loop {
            // Requeue imports a crashed worker left `in_progress`,
            // at startup and on a fixed cadence thereafter.
            if reclaim.due(Instant::now()) {
                reclaim_crashed(&state.pool).await;
            }
            let claimed = Box::pin(run_due(&state)).await;
            if retention.due(Instant::now()) {
                sweep(&state.pool).await;
            }
            if claimed == 0 && !crate::workers::pause(&state, IDLE_POLL).await {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use plamenu_db::account::{self, NewLocalAccount};
    use plamenu_db::bulk_import::ImportAdmission;

    use super::*;

    fn expect_admitted(admission: ImportAdmission) -> BulkImport {
        match admission {
            ImportAdmission::Admitted(import) => import,
            other => panic!("expected an admitted import, got {other:?}"),
        }
    }

    async fn seed_account(pool: &PgPool) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    /// The retention sweep removes stale imports (an unconfirmed one past 10
    /// minutes) while leaving fresh, still-`scheduled` work — the queue the
    /// worker is busy with does not block or lose retention. The
    /// `RetentionSchedule` unit test proves the sweep is time-driven, not
    /// idle-gated.
    #[sqlx::test(migrations = "../db/migrations")]
    async fn sweep_removes_stale_imports_only(pool: PgPool) {
        let alice = seed_account(&pool).await;

        // An unconfirmed import aged past the 10-minute window.
        let stale = expect_admitted(
            bulk_import::create_with_rows_capped(
                &pool,
                alice,
                "following",
                false,
                "a.csv",
                false,
                &[],
                100,
                i64::MAX,
            )
            .await
            .unwrap(),
        );
        sqlx::query!(
            "UPDATE bulk_imports SET created_at = now() - interval '11 minutes' WHERE id = $1",
            stale.id,
        )
        .execute(&pool)
        .await
        .unwrap();

        // A fresh, confirmed (`scheduled`) import — the pending queue — survives.
        let fresh = expect_admitted(
            bulk_import::create_with_rows_capped(
                &pool,
                alice,
                "following",
                false,
                "b.csv",
                false,
                &[],
                100,
                i64::MAX,
            )
            .await
            .unwrap(),
        );
        assert!(
            bulk_import::mark_scheduled(&pool, alice, fresh.id)
                .await
                .unwrap()
        );

        assert_eq!(sweep(&pool).await, 1);
        assert!(
            bulk_import::find_for_account(&pool, alice, stale.id)
                .await
                .unwrap()
                .is_none()
        );
        let survivor = bulk_import::find_for_account(&pool, alice, fresh.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(survivor.state, "scheduled");

        // Nothing stale remains.
        assert_eq!(sweep(&pool).await, 0);
    }
}
