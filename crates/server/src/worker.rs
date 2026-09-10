//! Shared helpers for the background workers.

use std::time::Duration;

use tokio::time::Instant;

/// Drives a periodic retention sweep on a fixed elapsed-time cadence,
/// independent of how busy the work queue is.
///
/// The archive and import workers poll their claim queue every couple of
/// seconds — and back-to-back while there is work to do. The previous design
/// only swept stale rows after 300 *consecutive idle* polls, and any claimed
/// batch reset that counter, so a continuously busy queue starved retention
/// forever: private account archives and stale imports could outlive their
/// documented retention windows indefinitely. This gates the
/// sweep on elapsed wall-clock time instead. The worker asks [`Self::due`] on
/// *every* loop iteration — busy or idle — and it returns `true` at most once
/// per `interval`. The schedule starts already due, so a sweep also runs at
/// startup.
pub struct RetentionSchedule {
    interval: Duration,
    due_at: Instant,
}

impl RetentionSchedule {
    /// A schedule that is due immediately (so a sweep runs at startup), then
    /// every `interval` thereafter.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            due_at: Instant::now(),
        }
    }

    /// Whether a sweep is due at `now`; when it is, arms the next deadline and
    /// returns `true`. The next window is anchored at `now` rather than the
    /// elapsed deadline, so a worker blocked far past a deadline runs a single
    /// catch-up sweep instead of a burst.
    pub fn due(&mut self, now: Instant) -> bool {
        if now >= self.due_at {
            self.due_at = now + self.interval;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_due_at_startup_then_once_per_interval() {
        let interval = Duration::from_mins(10);
        let mut schedule = RetentionSchedule::new(interval);
        // Captured just after construction, so it is at or after the schedule's
        // initial deadline: the first poll is due — retention runs at startup,
        // not after a warm-up of idle polls (the old idle-count gate never fired
        // under load at all). Subsequent instants are derived by arithmetic, so
        // the test is deterministic and does not depend on real time passing.
        let base = Instant::now();

        assert!(schedule.due(base));
        // Not due again until the interval elapses, however many times it is
        // polled meanwhile — a busy worker asks this every claim loop.
        assert!(!schedule.due(base));
        assert!(!schedule.due(base));
        assert!(!schedule.due(base + interval - Duration::from_secs(1)));
        // Due exactly once when the window passes...
        assert!(schedule.due(base + interval));
        assert!(!schedule.due(base + interval));
        // ...and re-armed for each subsequent window, anchored at the poll time.
        assert!(schedule.due(base + interval + interval));
    }
}
