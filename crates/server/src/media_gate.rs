//! Process-wide concurrency gate for heavy media work (ffmpeg re-encodes and
//! full-size image encodes). Before the gate, N concurrent uploads meant N
//! parallel encoders each free to use every core; now at most
//! [`configure`]d jobs run at once and the rest queue fairly.
//!
//! Probes, poster grabs and stream-copy remuxes stay ungated — they are
//! I/O-bound and cheap, and gating them behind long encodes would stall the
//! media proxy's interactive paths.

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

static GATE: OnceLock<Semaphore> = OnceLock::new();

/// Half the cores, at least one — enough to keep encodes off each other's
/// throats while leaving headroom for request serving.
fn auto_limit() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
}

/// Sets the gate width once, before any media work runs (`0` = auto:
/// half the cores). Later calls — and lazy first use — are no-ops if the
/// gate already exists, so a restart is needed for a new setting to apply.
pub fn configure(limit: usize) {
    let limit = if limit == 0 { auto_limit() } else { limit };
    let _ = GATE.set(Semaphore::new(limit));
}

/// Acquires one heavy-work slot, waiting behind other media jobs.
pub async fn acquire() -> SemaphorePermit<'static> {
    let gate = GATE.get_or_init(|| Semaphore::new(auto_limit()));
    gate.acquire().await.expect("media gate is never closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_works_without_configure() {
        let permit = acquire().await;
        drop(permit);
    }
}
