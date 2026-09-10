//! Worker lifecycle: cooperative shutdown and supervisor health.
//!
//! Every background worker is an infinite loop supervised by `main::supervise`.
//! Two shared pieces live here:
//!
//! * [`pause`] — the cooperative pause point worker loops use instead of a bare
//!   `sleep`. It returns `false` the moment shutdown is signalled, so a worker
//!   stops claiming new work and exits between jobs instead of being torn down
//!   mid-job at an arbitrary await point.
//! * [`WorkerRegistry`] — the supervisors' exit ledger. A healthy worker never
//!   exits, so recent exits are the readiness probe's signal that a subsystem
//!   is down or restart-looping.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::AppState;

/// Exits per worker within [`FAILURE_WINDOW`] before the readiness probe calls
/// the instance degraded. The supervisor respawns with a 1-second backoff, so
/// a crash *loop* clears this threshold within seconds, while an isolated
/// panic (respawned once, then healthy) never does.
pub const FAILURE_THRESHOLD: usize = 3;

/// How far back [`WorkerRegistry::restart_looping`] looks.
pub const FAILURE_WINDOW: Duration = Duration::from_mins(1);

/// Sleeps out `period` as a cooperative pause between work batches. Returns
/// `true` to keep looping, `false` when shutdown was signalled — the worker
/// should return promptly without claiming further work.
pub async fn pause(state: &AppState, period: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(period) => true,
        () = state.shutdown.cancelled() => false,
    }
}

/// The supervisors' shared exit ledger, read by the readiness probe.
#[derive(Default)]
pub struct WorkerRegistry {
    exits: Mutex<HashMap<&'static str, Vec<Instant>>>,
}

impl WorkerRegistry {
    /// Records that `name`'s task exited (panic or unexpected return). Called
    /// by the supervisor before it respawns.
    pub fn record_exit(&self, name: &'static str) {
        let now = Instant::now();
        let mut exits = self.exits.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = exits.entry(name).or_default();
        entries.retain(|at| now.duration_since(*at) < FAILURE_WINDOW);
        entries.push(now);
    }

    /// Workers that exited at least [`FAILURE_THRESHOLD`] times within
    /// [`FAILURE_WINDOW`] — i.e. subsystems currently restart-looping rather
    /// than recovering. Sorted for stable output.
    pub fn restart_looping(&self) -> Vec<&'static str> {
        let now = Instant::now();
        let mut exits = self.exits.lock().unwrap_or_else(PoisonError::into_inner);
        let mut looping: Vec<&'static str> = exits
            .iter_mut()
            .filter_map(|(name, entries)| {
                entries.retain(|at| now.duration_since(*at) < FAILURE_WINDOW);
                (entries.len() >= FAILURE_THRESHOLD).then_some(*name)
            })
            .collect();
        looping.sort_unstable();
        looping
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_exit_is_not_looping_but_repeated_exits_are() {
        let registry = WorkerRegistry::default();
        registry.record_exit("delivery");
        assert!(
            registry.restart_looping().is_empty(),
            "one crash is not a loop"
        );
        registry.record_exit("delivery");
        assert!(registry.restart_looping().is_empty());
        registry.record_exit("delivery");
        assert_eq!(registry.restart_looping(), vec!["delivery"]);
        // Another worker's exits are counted separately.
        registry.record_exit("mailer");
        assert_eq!(registry.restart_looping(), vec!["delivery"]);
    }
}
