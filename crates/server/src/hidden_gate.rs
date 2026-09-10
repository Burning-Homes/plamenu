//! Process-wide serialization for hidden-service (Tor/I2P) requests.
//!
//! Ported from Mitra's deliverer (`mitra_activitypub/src/deliverer.rs`):
//!
//! > Don't deliver to more than one onion at a time.
//! > Simultaneous requests frequently fail.
//!
//! Field-tested evidence that overlay-network requests are fragile under
//! concurrency, so at most one hidden-service delivery and one hidden-service
//! fetch are in flight at a time, however wide the delivery fan-out or the
//! remote-fetch pool is. Two separate one-permit gates rather than one shared:
//! a large media fetch may legitimately hold its slot for minutes and must not
//! stall the delivery queue behind it. Clearnet traffic never touches either
//! gate. Same [`OnceLock`]`<`[`Semaphore`]`>` pattern as
//! [`crate::crypto_gate`].

use std::sync::OnceLock;

use tokio::sync::{Semaphore, SemaphorePermit};

static DELIVERY_GATE: OnceLock<Semaphore> = OnceLock::new();
static FETCH_GATE: OnceLock<Semaphore> = OnceLock::new();

/// Whether `url` targets a hidden service — and therefore rides an overlay
/// circuit that wants serialization and forgiving failure accounting.
#[must_use]
pub fn is_hidden_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(plamenu_federation::is_hidden_service))
        .unwrap_or(false)
}

/// Acquires the single hidden-service *delivery* slot when `url` targets a
/// hidden service; returns `None` immediately for clearnet destinations.
pub async fn delivery_permit(url: &str) -> Option<SemaphorePermit<'static>> {
    permit(&DELIVERY_GATE, url).await
}

/// Acquires the single hidden-service *fetch* slot when `url` targets a
/// hidden service; returns `None` immediately for clearnet destinations.
/// Acquire this **before** the remote-fetch admission permit, so queued
/// hidden fetches wait here without occupying slots of the clearnet pool.
pub async fn fetch_permit(url: &str) -> Option<SemaphorePermit<'static>> {
    permit(&FETCH_GATE, url).await
}

async fn permit(gate: &'static OnceLock<Semaphore>, url: &str) -> Option<SemaphorePermit<'static>> {
    if !is_hidden_url(url) {
        return None;
    }
    let gate = gate.get_or_init(|| Semaphore::new(1));
    Some(
        gate.acquire()
            .await
            .expect("hidden-service gate is never closed"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_urls_are_detected() {
        assert!(is_hidden_url("http://xyz.onion/inbox"));
        assert!(is_hidden_url("http://xyz.i2p/inbox"));
        assert!(!is_hidden_url("https://remote.example/inbox"));
        assert!(!is_hidden_url("https://onion.example.com/inbox"));
        assert!(!is_hidden_url("not a url"));
    }

    #[tokio::test]
    async fn gate_serializes_hidden_and_ignores_clearnet() {
        assert!(
            delivery_permit("https://remote.example/inbox")
                .await
                .is_none()
        );

        let first = delivery_permit("http://xyz.onion/inbox").await;
        assert!(first.is_some());
        // While the first permit is held, a second hidden delivery must wait…
        let second = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            delivery_permit("http://abc.onion/inbox"),
        )
        .await;
        assert!(second.is_err(), "second hidden delivery should block");
        // …and clearnet (and the separate fetch gate) stay unaffected.
        assert!(
            delivery_permit("https://remote.example/inbox")
                .await
                .is_none()
        );
        assert!(fetch_permit("http://xyz.onion/media").await.is_some());

        drop(first);
        let unblocked = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            delivery_permit("http://abc.onion/inbox"),
        )
        .await;
        assert!(unblocked.is_ok_and(|permit| permit.is_some()));
    }
}
