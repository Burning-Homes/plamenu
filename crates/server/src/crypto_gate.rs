//! Process-wide concurrency gate for CPU/memory-heavy credential crypto —
//! Argon2id password hashing/verification and RSA-2048 actor-key generation.
//!
//! These run on Tokio's blocking pool (never inline on an async worker thread),
//! but the blocking pool alone is not enough: Argon2id deliberately costs tens
//! of megabytes and RSA keygen is CPU-bound, so a burst of accepted
//! logins/registrations/group-creations could spawn hundreds of them at once and
//! starve the runtime and the background workers sharing it. This
//! gate caps how many run concurrently — the rest queue fairly — mirroring the
//! media codec gate ([`crate::media_gate`]). It bounds *work concurrency*, not
//! *attempt volume*: per-IP/e-mail login budgets and account lockout stay as
//! separate layers.

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

static GATE: OnceLock<Semaphore> = OnceLock::new();

/// Half the cores, at least one — enough parallel credential crypto to serve a
/// healthy login/registration rate while leaving cores for request serving and
/// workers, so a flood cannot monopolize the runtime.
fn auto_limit() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
}

/// Sets the gate width once, before any credential crypto runs (`0` = auto:
/// half the cores). Later calls — and lazy first use — are no-ops if the gate
/// already exists, so a restart is needed for a new setting to apply.
pub fn configure(limit: usize) {
    let limit = if limit == 0 { auto_limit() } else { limit };
    let _ = GATE.set(Semaphore::new(limit));
}

/// Acquires one credential-crypto slot, waiting behind other in-flight
/// hashes/verifications/keygens.
pub async fn acquire() -> SemaphorePermit<'static> {
    let gate = GATE.get_or_init(|| Semaphore::new(auto_limit()));
    gate.acquire().await.expect("crypto gate is never closed")
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
